//! Decides whether a manifest, lockfile or tool configuration lets an install run repository
//! code or reach outside the checkout.
use super::*;

/// Whether a pip requirements file names only packages from an index, so that
/// `--only-binary :all:` leaves pip nothing to build from source. A file may itself set
/// `--no-binary` or `--only-binary :none:`, include other files (`-r`, `-c`), name an editable
/// or a direct reference (a URL, a path, an archive) or pass build settings, and pip expands
/// environment variables in it; any of those could build a package, so such a file needs
/// `allowScripts`.
pub(super) fn binary_only_requirements(manifest: &[u8]) -> bool {
    // (option, takes a value)
    const GLOBAL: &[(&str, bool)] = &[
        ("-i", true),
        ("--index-url", true),
        ("--extra-index-url", true),
        ("--no-index", false),
        ("-f", true),
        ("--find-links", true),
        ("--prefer-binary", false),
        ("--require-hashes", false),
        ("--pre", false),
        ("--trusted-host", true),
    ];
    const PER_REQUIREMENT: &[(&str, bool)] = &[("--hash", true)];
    // Every final suffix of a source archive pip recognizes (.zip, .tar, .tar.gz, .tgz,
    // .tar.bz2, .tbz, .tar.xz, .txz, .tlz, .tar.lz, .tar.lzma), and .tbz2 as well.
    const ARCHIVES: &[&str] = &[
        ".zip", ".tar", ".gz", ".tgz", ".bz2", ".tbz", ".tbz2", ".xz", ".txz", ".tlz", ".lz",
        ".lzma",
    ];
    let Ok(text) = std::str::from_utf8(manifest) else {
        return false;
    };
    let text = text.replace("\\\r\n", "").replace("\\\n", "");
    text.lines().all(|line| {
        // pip drops a comment that starts the line or follows whitespace.
        let mut code = line;
        let mut previous = None;
        for (index, character) in line.char_indices() {
            if character == '#' && previous.is_none_or(char::is_whitespace) {
                code = &line[..index];
                break;
            }
            previous = Some(character);
        }
        let code = code.trim();
        if code.is_empty() {
            return true;
        }
        if code.contains('$') {
            return false;
        }
        // Like pip, the options start at the first token that starts with `-`; every one of
        // them must be allowed, not just the first.
        let tokens: Vec<&str> = code.split_whitespace().collect();
        let first_option = tokens
            .iter()
            .position(|token| token.starts_with('-'))
            .unwrap_or(tokens.len());
        let (requirement, options) = tokens.split_at(first_option);
        if requirement.is_empty() {
            return allowed_options(options, GLOBAL);
        }
        let requirement = requirement.join(" ");
        let requirement = requirement.split(';').next().unwrap_or_default().trim();
        // The project name ends where its extras, version specifier or a space start; pip
        // strips extras before it recognizes a local archive.
        let name = requirement
            .split(['[', '<', '>', '=', '!', '~', ' ', '('])
            .next()
            .unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            && !requirement.contains(['@', '/', '\\', ':'])
            && !ARCHIVES.iter().any(|archive| lower.ends_with(archive))
            && allowed_options(options, PER_REQUIREMENT)
    })
}

/// Whether the manager would run a program the repository configures before (or instead of)
/// the install itself, which no install option stops; such an install needs `allowScripts`.
///
/// - Yarn: a Yarn 1 `.yarnrc`, or a `.yarnrc.yml` that [`inert_yarn_config`] does not accept,
///   in the repository root or in the install's home (inside the repository).
/// - Cargo: a Cargo configuration in the repository root (`.cargo/config` or
///   `.cargo/config.toml`) with any setting outside [`inert_cargo_config`]'s allow-list, which
///   could name the compiler, a wrapper or a credential provider that even `cargo fetch` runs;
///   any configuration in the old in-repository `CARGO_HOME`; and a toolchain file naming a
///   toolchain by path (a TOML key spelled with an escape needs `\`, so a file with `path`, `/`
///   or `\` counts).
/// - npm, pnpm and Yarn: a dependency source that runs repository code or reads outside the
///   checkout (`exec:`, git, `file:`/`link:`/`portal:` leaving the checkout) in any
///   `package.json` or the lockfile (see [`node_dependency_source_unchecked`]).
/// - npm, pnpm and Yarn: a `package.json` `packageManager` that is not a plain
///   `name@version`, since Corepack (behind a manager shim) downloads and runs the release a
///   URL there names; and a `package.json` that does not parse, whose field cannot be checked.
pub(super) fn install_runs_repository_code(name: &str, root: &Path) -> bool {
    let present = |path: PathBuf| match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        // An entry that cannot be inspected cannot be ruled out.
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    };
    let node = matches!(
        name,
        "package-lock.json" | "package.json" | "pnpm-lock.yaml" | "yarn.lock"
    );
    if node && (corepack_release_unchecked(root) || node_dependency_source_unchecked(name, root)) {
        return true;
    }
    match name {
        "yarn.lock" => {
            yarn_ancestor_rc_present(root)
                || [root.to_path_buf(), root.join(".devworks-home")]
                    .iter()
                    .any(|directory| {
                        present(directory.join(".yarnrc"))
                            || match std::fs::symlink_metadata(directory.join(".yarnrc.yml")) {
                                Ok(metadata) => {
                                    // A link, or a file too large to check, cannot be ruled out.
                                    !metadata.file_type().is_file()
                                        || metadata.len() > 1024 * 1024
                                        || std::fs::read(directory.join(".yarnrc.yml"))
                                            .map_or(true, |bytes| !inert_yarn_config(&bytes))
                                }
                                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                            }
                    })
        }
        "Cargo.toml" | "Cargo.lock" => {
            let repository_config =
                ["config", "config.toml"].iter().any(|file| {
                    match std::fs::read(root.join(".cargo").join(file)) {
                        Ok(bytes) => !inert_cargo_config(&bytes),
                        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                    }
                });
            let old_home_config = ["config", "config.toml"]
                .iter()
                .any(|file| present(root.join(".devworks-cache/cargo").join(file)));
            let configured = repository_config || old_home_config;
            let toolchain_path = ["rust-toolchain", "rust-toolchain.toml"]
                .iter()
                .any(|file| match std::fs::read(root.join(file)) {
                    Ok(text) => {
                        let text = String::from_utf8_lossy(&text);
                        text.contains("path") || text.contains(['/', '\\'])
                    }
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                });
            configured || toolchain_path
        }
        _ => false,
    }
}

/// Whether a directory above the checkout holds a Yarn rc file. Yarn reads `.yarnrc.yml` from
/// every ancestor of the working directory and has no environment variable or flag that stops
/// that lookup (`YARN_RC_FILENAME` renames the file for the checkout too), so the check is
/// here. A folder that cannot be inspected counts as one with a file.
pub(super) fn yarn_ancestor_rc_present(root: &Path) -> bool {
    // Every ancestor counts: Yarn reads them all, and which of them a repository can write is
    // not known here. A person's own rc file above the checkout only costs the script-free path.
    for directory in root.ancestors().skip(1) {
        for file in [".yarnrc.yml", ".yarnrc"] {
            match std::fs::symlink_metadata(directory.join(file)) {
                Ok(_) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return true,
            }
        }
    }
    false
}

/// Whether a repository's `.yarnrc.yml` holds only settings that name no program Yarn would
/// load or run and inject no code or environment. It fails closed: a file that is not UTF-8 or
/// does not parse, and any key not listed here, counts as naming one.
///
/// The file is read as YAML events, not as values, so nothing is resolved before it is
/// checked: a document with an anchor, an alias, a `<<` merge key, an explicit tag, a duplicate
/// key, a non-scalar key, a second document or a top level that is not a mapping is refused,
/// since another YAML reader could resolve it to a different key than this one does.
///
/// Allowed (inert): `nodeLinker` (`node-modules`, `pnpm` or `pnp`), `npmRegistryServer` (an
/// `http(s)` URL), `npmScopes` (per scope: `npmRegistryServer`, `npmPublishRegistry`,
/// `npmAlwaysAuth`, `npmAuthToken`, `npmAuthIdent`), `enableGlobalCache`, `compressionLevel`,
/// `enableTelemetry`, `enableMirror`, `enableNetwork`, `enableOfflineMode`,
/// `enableImmutableInstalls`, `enableColors`, `enableProgressBars`, `enableMessageNames`,
/// `enableTimers`, `enableHyperlinks`, `enableTransparentWorkspaces`, `httpTimeout`,
/// `httpRetry`, `networkConcurrency`, `httpProxy`, `httpsProxy`, `defaultSemverRangePrefix`,
/// `defaultProtocol`, `nmMode`, `nmHoistingLimits`, `nmSelfReferences`, `winLinkType`,
/// `preferTruncatedLines`, and the folders `cacheFolder` and `globalFolder` (every script-free
/// install sets `YARN_CACHE_FOLDER` and `YARN_GLOBAL_FOLDER` to folders in the host's home, and
/// an environment variable overrides the file).
///
/// Refused: `yarnPath`, `plugins`, `injectEnvironmentFiles`, `constraintsPath`,
/// `packageExtensions` (adds dependencies), `enableScripts` and `ignorePath` (the install sets
/// both), `pnpUnpluggedFolder`, `virtualFolder`, `installStatePath`, `lockfileFilename` and
/// other paths in the checkout, `enableStrictSsl`, `unsafeHttpWhitelist`, `caFilePath` and
/// `checksumBehavior` (weaken verification), and everything not listed above.
pub(super) fn inert_yarn_config(bytes: &[u8]) -> bool {
    use yaml_rust2::{
        parser::{Event, MarkedEventReceiver, Parser, Tag},
        scanner::Marker,
    };
    #[derive(Default)]
    struct Events(Vec<Event>);
    impl MarkedEventReceiver for Events {
        fn on_event(&mut self, event: Event, _mark: Marker) {
            self.0.push(event);
        }
    }
    enum Node {
        Scalar(String),
        Map(Vec<(String, Node)>),
        Seq,
    }
    fn node(events: &mut std::slice::Iter<Event>, depth: usize) -> Option<Node> {
        let plain = |anchor: &usize, tag: &Option<Tag>| *anchor == 0 && tag.is_none();
        if depth > 8 {
            return None;
        }
        match events.next()? {
            // Yarn expands `${VAR}` from the environment, which would leak host paths.
            Event::Scalar(value, _, anchor, tag) if plain(anchor, tag) && !value.contains("${") => {
                Some(Node::Scalar(value.clone()))
            }
            Event::SequenceStart(anchor, tag) if plain(anchor, tag) => {
                while !matches!(events.as_slice().first()?, Event::SequenceEnd) {
                    node(events, depth + 1)?;
                }
                events.next();
                Some(Node::Seq)
            }
            Event::MappingStart(anchor, tag) if plain(anchor, tag) => {
                let mut entries: Vec<(String, Node)> = Vec::new();
                while !matches!(events.as_slice().first()?, Event::MappingEnd) {
                    let Node::Scalar(key) = node(events, depth + 1)? else {
                        return None;
                    };
                    if key == "<<" || entries.iter().any(|(seen, _)| *seen == key) {
                        return None;
                    }
                    entries.push((key, node(events, depth + 1)?));
                }
                events.next();
                Some(Node::Map(entries))
            }
            _ => None,
        }
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    // Yarn's parser switches to its legacy lockfile reader when a leading comment says
    // `yarn lockfile v1`, while the YAML reader here sees only comments.
    let lowered = text.to_ascii_lowercase();
    let words: Vec<&str> = lowered.split_whitespace().collect();
    if words.windows(2).any(|pair| pair == ["lockfile", "v1"]) {
        return false;
    }
    let mut receiver = Events::default();
    if Parser::new_from_str(text)
        .load(&mut receiver, true)
        .is_err()
    {
        return false;
    }
    let mut events = receiver.0.iter();
    let mut documents = 0;
    let mut root = None;
    while let Some(event) = events.next() {
        match event {
            Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return false;
                }
                let Some(top) = node(&mut events, 0) else {
                    return false;
                };
                root = Some(top);
            }
            _ => return false,
        }
    }
    let entries = match root {
        None => return true,
        // An empty file (or only comments) parses as a document with an empty value.
        Some(Node::Scalar(value)) => return value.is_empty(),
        Some(Node::Map(entries)) => entries,
        Some(Node::Seq) => return false,
    };
    let scalar = |value: &Node| matches!(value, Node::Scalar(_));
    let registry = |value: &Node| {
        matches!(value, Node::Scalar(url)
            if url.starts_with("https://") || url.starts_with("http://"))
    };
    const SCALARS: &[&str] = &[
        "enableGlobalCache",
        "compressionLevel",
        "enableTelemetry",
        "enableMirror",
        "enableNetwork",
        "enableOfflineMode",
        "enableImmutableInstalls",
        "enableColors",
        "enableProgressBars",
        "enableMessageNames",
        "enableTimers",
        "enableHyperlinks",
        "enableTransparentWorkspaces",
        "httpTimeout",
        "httpRetry",
        "networkConcurrency",
        "httpProxy",
        "httpsProxy",
        "defaultSemverRangePrefix",
        "nmMode",
        "nmHoistingLimits",
        "nmSelfReferences",
        "winLinkType",
        "preferTruncatedLines",
        "cacheFolder",
        "globalFolder",
    ];
    entries.iter().all(|(key, value)| match key.as_str() {
        key if SCALARS.contains(&key) => scalar(value),
        // Yarn prepends this to every bare range, so any other protocol (`exec:`, `file:`)
        // would make ordinary dependencies resolve to repository files.
        "defaultProtocol" => matches!(value, Node::Scalar(protocol) if protocol == "npm:"),
        "nodeLinker" => {
            matches!(value, Node::Scalar(linker) if matches!(linker.as_str(), "node-modules" | "pnpm" | "pnp"))
        }
        "npmRegistryServer" => registry(value),
        "npmScopes" => match value {
            Node::Map(scopes) => scopes.iter().all(|(_, scope)| match scope {
                Node::Map(settings) => settings.iter().all(|(key, value)| match key.as_str() {
                    "npmRegistryServer" | "npmPublishRegistry" => registry(value),
                    "npmAlwaysAuth" | "npmAuthToken" | "npmAuthIdent" => scalar(value),
                    _ => false,
                }),
                _ => false,
            }),
            _ => false,
        },
        _ => false,
    })
}

/// Whether a repository's Cargo configuration file holds only settings that name no program a
/// script-free install (`cargo fetch`) could run. It fails closed: a file that does not parse,
/// and any table or key not listed here, counts as naming one.
///
/// Allowed: `[alias]` (Cargo ignores an alias that shadows a built-in command such as `fetch`),
/// `[cargo-new]`, `[future-incompat-report]`, `[cache]`, `[gc]`, `[http]`, `[patch]`, `[term]`,
/// `[resolver]`, `[install]`, `paths`; `build.jobs`, `build.target-dir`, `build.incremental`,
/// `build.dep-info-basedir`, a `build.target` that is a plain target name (not a `.json` target
/// specification), and `build.rustflags` of [`inert_rustflags`]; `target.<triple or cfg>`
/// holding only such `rustflags`; `net.retry`, `net.offline`, `net.git-fetch-with-cli` (it runs
/// the host's `git`, under the script-free Git overrides, in Cargo's own database) and `net.ssh`;
/// `[profile]` without `rustflags` or `codegen-backend`; `registry.default`, `registry.token`;
/// `registries.<name>.index`, `.token`, `.protocol`; and `[source]` replacement (`replace-with`,
/// `directory`, `registry`, `local-registry`, `git`, `branch`, `tag`, `rev`).
///
/// Everything else counts, notably `build.rustc`, `build.rustc-wrapper`,
/// `build.rustc-workspace-wrapper`, `build.rustdoc`, `target.*.runner`, `target.*.linker`,
/// `[host]`, `[env]` (it can set variables such as `LD_PRELOAD` for the compiler Cargo queries),
/// `[unstable]`, `include`, `doc.browser`, `registry.credential-provider`,
/// `registry.global-credential-providers`, `registries.*.credential-provider` and
/// `[credential-alias]`.
pub(super) fn inert_cargo_config(bytes: &[u8]) -> bool {
    use toml::Value;
    let Some(config) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
    else {
        return false;
    };
    let only = |value: &Value, keys: &[&str]| {
        value
            .as_table()
            .is_some_and(|table| table.keys().all(|key| keys.contains(&key.as_str())))
    };
    let each = |value: &Value, keys: &[&str]| {
        value
            .as_table()
            .is_some_and(|table| table.values().all(|entry| only(entry, keys)))
    };
    let plain_target = |value: &Value| {
        let name = |value: &Value| {
            value.as_str().is_some_and(|name| {
                !name.is_empty()
                    && !name.to_ascii_lowercase().ends_with(".json")
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            })
        };
        name(value) || value.as_array().is_some_and(|names| names.iter().all(name))
    };
    fn compiler_settings(value: &Value) -> bool {
        value.as_table().is_some_and(|table| {
            table.iter().any(|(key, value)| {
                matches!(key.as_str(), "rustflags" | "codegen-backend") || compiler_settings(value)
            })
        })
    }
    config.iter().all(|(key, value)| match key.as_str() {
        "alias"
        | "cargo-new"
        | "future-incompat-report"
        | "cache"
        | "gc"
        | "http"
        | "patch"
        | "term"
        | "resolver"
        | "install"
        | "paths" => true,
        "build" => value.as_table().is_some_and(|build| {
            build.iter().all(|(key, value)| match key.as_str() {
                "jobs" | "target-dir" | "incremental" | "dep-info-basedir" => true,
                "target" => plain_target(value),
                "rustflags" => inert_rustflags(value),
                _ => false,
            })
        }),
        "target" => value.as_table().is_some_and(|targets| {
            targets.values().all(|target| {
                target.as_table().is_some_and(|target| {
                    target
                        .iter()
                        .all(|(key, value)| key == "rustflags" && inert_rustflags(value))
                })
            })
        }),
        "net" => only(value, &["retry", "offline", "git-fetch-with-cli", "ssh"]),
        "profile" => value.is_table() && !compiler_settings(value),
        "registry" => only(value, &["default", "token"]),
        "registries" => each(value, &["index", "token", "protocol"]),
        "source" => each(
            value,
            &[
                "replace-with",
                "directory",
                "registry",
                "local-registry",
                "git",
                "branch",
                "tag",
                "rev",
            ],
        ),
        _ => false,
    })
}

/// Whether Cargo `rustflags` (a string of space-separated flags or an array of flags) are only
/// lint levels (`-D`, `-W`, `-A`, `-F`, `--cap-lints`), `--cfg`/`--check-cfg`, `-g`, `-O`, or
/// codegen options (`-C`, `--codegen`) that choose how code is generated (`target-cpu`,
/// `target-feature`, `opt-level`, `debuginfo`, `force-frame-pointers`, `overflow-checks`,
/// `panic`, `relocation-model`, `code-model`, `strip`, `symbol-mangling-version`,
/// `split-debuginfo`, `embed-bitcode`, `lto`, `codegen-units`, `debug-assertions`). Cargo passes
/// them to the compiler it queries during `cargo fetch`; anything else (`-Z`, an `@file`,
/// `--sysroot`, `linker`, `codegen-backend`, `link-arg`) counts as naming a program. Values are
/// checked too: a lint name, a `--cap-lints` level, a `--cfg` of `name` or `name="value"`, a
/// `--check-cfg` of `cfg(...)`, and a codegen value of letters, digits and `+-_.,`, so no
/// element anywhere can be an `@file` that rustc would expand into other flags.
pub(super) fn inert_rustflags(value: &toml::Value) -> bool {
    const CODEGEN: &[&str] = &[
        "target-cpu",
        "target-feature",
        "opt-level",
        "debuginfo",
        "force-frame-pointers",
        "overflow-checks",
        "panic",
        "relocation-model",
        "code-model",
        "strip",
        "symbol-mangling-version",
        "split-debuginfo",
        "embed-bitcode",
        "lto",
        "codegen-units",
        "debug-assertions",
    ];
    let flags: Vec<&str> = match value {
        toml::Value::String(flags) => flags.split_whitespace().collect(),
        toml::Value::Array(flags) => {
            let Some(flags) = flags.iter().map(toml::Value::as_str).collect::<Option<_>>() else {
                return false;
            };
            flags
        }
        _ => return false,
    };
    // Every value is checked as strictly as a flag: rustc expands any argument that starts with
    // `@` (a response file) before it parses options, so no element may start with `@` or `-`
    // where a value is expected.
    let identifier = |name: &str| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    // `name` or `name="value"`, with no quote or backslash inside the value.
    let cfg = |spec: &str| match spec.split_once('=') {
        None => identifier(spec),
        Some((name, value)) => {
            identifier(name)
                && value.len() >= 2
                && value.starts_with('"')
                && value.ends_with('"')
                && !value[1..value.len() - 1].contains(['"', '\\'])
        }
    };
    // `cfg(...)` of names, string literals, commas, spaces and `values`/`any()`.
    let check_cfg = |spec: &str| {
        spec.starts_with("cfg(")
            && spec.ends_with(')')
            && spec.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '_' | '(' | ')' | ',' | ' ' | '"' | '.' | '-')
            })
    };
    let lint = |name: &str| {
        name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
    };
    let cap = |level: &str| matches!(level, "allow" | "warn" | "deny" | "forbid");
    // An allowed codegen option, with a value of letters, digits and `+-_.,` only.
    let codegen = |option: &str| {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        CODEGEN.contains(&key)
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '.' | ','))
    };
    let mut flags = flags.into_iter();
    while let Some(flag) = flags.next() {
        let mut value = |check: &dyn Fn(&str) -> bool| flags.next().is_some_and(check);
        let inert = match flag {
            "-g" | "-O" => true,
            "-D" | "-W" | "-A" | "-F" => value(&lint),
            "--cap-lints" => value(&cap),
            "--cfg" => value(&cfg),
            "--check-cfg" => value(&check_cfg),
            "-C" | "--codegen" => value(&codegen),
            _ => {
                if let Some(option) = flag
                    .strip_prefix("--codegen=")
                    .or_else(|| flag.strip_prefix("-C"))
                {
                    codegen(option)
                } else if let Some(name) = ["-D", "-W", "-A", "-F"]
                    .iter()
                    .find_map(|prefix| flag.strip_prefix(prefix))
                {
                    lint(name)
                } else if let Some(spec) = flag.strip_prefix("--cfg=") {
                    cfg(spec)
                } else if let Some(spec) = flag.strip_prefix("--check-cfg=") {
                    check_cfg(spec)
                } else if let Some(level) = flag.strip_prefix("--cap-lints=") {
                    cap(level)
                } else {
                    false
                }
            }
        };
        if !inert {
            return false;
        }
    }
    true
}

/// Whether the `packageManager` of the checkout's `package.json` could name a release other
/// than a registry version: a value that is not `name@version` (letters, digits and `.+-_`
/// only, so no URL), or a file that cannot be read or parsed.
pub(super) fn corepack_release_unchecked(root: &Path) -> bool {
    let bytes = match std::fs::read(root.join("package.json")) {
        Ok(bytes) => bytes,
        Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
    };
    let Ok(Value::Object(package)) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    match package.get("packageManager") {
        None => false,
        Some(Value::String(spec)) => !spec.split_once('@').is_some_and(|(name, version)| {
            !name.is_empty()
                && !version.is_empty()
                && format!("{name}{version}")
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
        }),
        Some(_) => true,
    }
}

/// Whether a Node project's dependencies come from a source that runs repository code or reads
/// outside the checkout, so a script-free install is refused. It looks at every `package.json`
/// in the checkout (any one could be a workspace; `node_modules` and `.git` are skipped, and a
/// tree of more than 50,000 entries, a symlinked folder or file that cannot be read or parsed
/// is refused, so it fails closed) and at the lockfile `name`, which is repository-controlled
/// and can name a source the manifests do not.
///
/// Decided per source (see [`unsafe_dependency_source`]):
/// - `exec:` (Yarn 2+) runs a repository file to build the package, whatever
///   `YARN_ENABLE_SCRIPTS` says: refused.
/// - Git sources (`git:`, `git+…`, `github:`, `gitlab:`, `bitbucket:`, `gist:`, `ssh:`,
///   `git@host:…`, a URL ending in `.git`, a URL on a git host, `owner/repo`): refused. Yarn
///   packs a git dependency through a nested install that runs its `prepack` and `prepare`
///   scripts, and npm runs `prepare` for one; neither is known to honour the script-free
///   switch for that nested step, so it is not relied on.
/// - `file:`, `link:` and `portal:` (and a bare `./`, `../`, `/` or `~/` path) are refused when
///   the path is absolute, starts with `~`, names a drive, or leaves the checkout; one inside
///   the checkout installs, since copying or linking a folder runs nothing with scripts off.
/// - `patch:` applies a patch file with Yarn's own patcher and runs no code, so it is allowed;
///   the source it patches is checked like any other (`patch:x@exec:…` is refused).
/// - `npm:` aliases and registry ranges, `workspace:`, `catalog:` and `http(s)` tarballs install.
/// - Protocols that plugins add are refused already, since plugins are.
///
/// Manifest fields read: `dependencies`, `devDependencies`, `optionalDependencies`,
/// `peerDependencies`, `resolutions`, `overrides`, `pnpm.overrides`, `pnpm.packageExtensions`,
/// `pnpm.patchedDependencies` (patch paths must stay in the checkout), `workspaces.catalog(s)`
/// and the `workspaces` globs (which must stay in the checkout). Also read: `npm-shrinkwrap.json`
/// beside `package-lock.json`, and `pnpm-workspace.yaml` (`packages`, `overrides`, `catalog(s)`,
/// `packageExtensions`, `patchedDependencies`; anchors, aliases and tags are refused).
///
/// `package-lock.json` and `npm-shrinkwrap.json` are parsed as JSON and every string is checked,
/// as are `resolved` paths and `packages` locations. `yarn.lock` and `pnpm-lock.yaml` are read
/// by line, failing closed: any backslash other than in `\"` is refused (it could hide a
/// source behind `\u0065xec` or a line continuation), each line is cut into tokens at
/// whitespace, quotes, commas and brackets, each token is percent-decoded (Yarn encodes the
/// source of a `patch:`), and the token and the text after each `@` in it are checked, as is
/// the path after a `directory` or `resolved` key. A relative `link:`, `file:` or `portal:`
/// path is resolved against its pnpm importer or Berry workspace (the `importers` key, the
/// `@workspace:` path of the block or of a `::locator=`); where neither is named it counts from
/// the checkout root, so any `..` is refused. A token that is only `exec:` (a dependency named
/// `exec`) is not a source.
pub(super) fn node_dependency_source_unchecked(name: &str, root: &Path) -> bool {
    if !matches!(
        name,
        "package-lock.json" | "package.json" | "pnpm-lock.yaml" | "yarn.lock"
    ) {
        return false;
    }
    // npm prefers `npm-shrinkwrap.json` to `package-lock.json`; pnpm reads its workspace file.
    let scanned: &[&str] = match name {
        "package-lock.json" => &["package-lock.json", "npm-shrinkwrap.json"],
        "pnpm-lock.yaml" => &["pnpm-lock.yaml", "pnpm-workspace.yaml"],
        "yarn.lock" => &["yarn.lock"],
        _ => &[],
    };
    for file in scanned {
        match std::fs::metadata(root.join(file)) {
            Ok(metadata) if metadata.len() <= 128 * 1024 * 1024 => {
                let Ok(bytes) = std::fs::read(root.join(file)) else {
                    return true;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    return true;
                };
                let unsafe_file = match *file {
                    "pnpm-workspace.yaml" => pnpm_workspace_unsafe(&text),
                    _ => lockfile_unsafe(file, &text),
                };
                if unsafe_file {
                    return true;
                }
            }
            // The caller read the lockfile; a missing one has nothing to scan.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return true,
        }
    }
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return true;
        };
        for entry in entries {
            seen += 1;
            let Ok(entry) = entry else { return true };
            if seen > 50_000 {
                return true;
            }
            let Ok(kind) = entry.file_type() else {
                return true;
            };
            let file_name = entry.file_name();
            if kind.is_symlink() {
                // A link to a folder may be a workspace outside the checkout.
                if std::fs::metadata(entry.path()).map_or(true, |m| m.is_dir())
                    || file_name == "package.json"
                {
                    return true;
                }
            } else if kind.is_dir() {
                if file_name != "node_modules" && file_name != ".git" {
                    pending.push((entry.path(), depth + 1));
                }
            } else if file_name == "package.json" {
                let path = entry.path();
                match std::fs::metadata(&path) {
                    Ok(metadata) if metadata.len() <= 4 * 1024 * 1024 => {}
                    _ => return true,
                }
                match std::fs::read(&path) {
                    Ok(bytes) if package_json_unsafe(&bytes, depth) => return true,
                    Ok(_) => {}
                    Err(_) => return true,
                }
            }
        }
    }
    false
}

/// Whether the dependency fields of a `package.json` (at `depth` folders below the checkout)
/// name a source [`unsafe_dependency_source`] refuses, a patch file or workspace glob outside the
/// checkout, or the file does not parse.
pub(super) fn package_json_unsafe(bytes: &[u8], depth: usize) -> bool {
    fn any_string(value: &Value, test: &dyn Fn(&str) -> bool) -> bool {
        match value {
            Value::String(text) => test(text),
            Value::Array(items) => items.iter().any(|item| any_string(item, test)),
            Value::Object(map) => map.values().any(|item| any_string(item, test)),
            _ => false,
        }
    }
    let Ok(Value::Object(package)) = serde_json::from_slice::<Value>(bytes) else {
        return true;
    };
    let test = |text: &str| unsafe_dependency_source(text, depth, true);
    let pnpm = package.get("pnpm");
    let fields = [
        package.get("dependencies"),
        package.get("devDependencies"),
        package.get("optionalDependencies"),
        package.get("peerDependencies"),
        package.get("resolutions"),
        package.get("overrides"),
        pnpm.and_then(|pnpm| pnpm.get("overrides")),
        pnpm.and_then(|pnpm| pnpm.get("packageExtensions")),
        package.get("workspaces").and_then(|w| w.get("catalog")),
        package.get("workspaces").and_then(|w| w.get("catalogs")),
    ];
    if fields
        .into_iter()
        .flatten()
        .any(|field| any_string(field, &test))
    {
        return true;
    }
    // Patch files are paths relative to the `package.json`. (`bundleDependencies` holds names.)
    if let Some(patches) = pnpm.and_then(|pnpm| pnpm.get("patchedDependencies"))
        && any_string(patches, &|path| path_leaves_checkout(path, depth))
    {
        return true;
    }
    let globs = match package.get("workspaces") {
        Some(Value::Object(map)) => map.get("packages"),
        other => other,
    };
    globs.is_some_and(|globs| any_string(globs, &|glob| glob_leaves_checkout(glob, depth)))
}

/// Whether a workspace glob can match outside the checkout: absolute, a drive or home folder,
/// or `..` segments that climb past it (or follow a wildcard, which could match nothing).
pub(super) fn glob_leaves_checkout(glob: &str, depth: usize) -> bool {
    let glob = glob.trim().trim_start_matches('!');
    if glob.starts_with(['/', '\\', '~']) || glob.contains(['\\', ':']) {
        return true;
    }
    let (mut depth, mut wild) = (depth, false);
    for segment in glob.split('/') {
        match segment {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(rest) if !wild => depth = rest,
                _ => return true,
            },
            _ => {
                wild |= segment.contains(['*', '?', '[', '{', '(']);
                depth += 1;
            }
        }
    }
    false
}

/// How many folders below the checkout the workspace in a Berry locator or descriptor
/// (`name@workspace:path`, possibly percent-encoded) is; `None` when it names none, and
/// `Some(None)` when its path leaves the checkout.
pub(super) fn workspace_depth(text: &str) -> Option<Option<usize>> {
    let mut best: Option<Option<usize>> = None;
    for (index, _) in text.match_indices("@workspace:") {
        let path = &text[index + "@workspace:".len()..];
        let path = path
            .split(['"', '\'', ',', ':', '#', ' '])
            .next()
            .unwrap_or("");
        let depth = if path_leaves_checkout(path, 0) {
            None
        } else {
            Some(
                path.split('/')
                    .filter(|s| !matches!(*s, "" | "."))
                    .fold(0, |d: usize, s| {
                        if s == ".." {
                            d.saturating_sub(1)
                        } else {
                            d + 1
                        }
                    }),
            )
        };
        best = match (best, depth) {
            (None, found) => Some(found),
            (Some(None), _) | (_, None) => Some(None),
            (Some(Some(a)), Some(b)) => Some(Some(a.min(b))),
        };
    }
    best
}

/// Whether a lockfile names a source [`unsafe_dependency_source`] refuses (see
/// [`node_dependency_source_unchecked`] for how it is read). `name` picks the reader: npm's JSON
/// lockfiles are parsed and every string checked; the Yarn and pnpm YAML-like text is read by
/// line, and any backslash other than in `\"` is refused, since a quoted string could hide a
/// source behind `\u0065xec` or a `\`-newline continuation.
pub(super) fn lockfile_unsafe(name: &str, text: &str) -> bool {
    if matches!(name, "package-lock.json" | "npm-shrinkwrap.json") {
        return match serde_json::from_str::<Value>(text) {
            Ok(value) => npm_lockfile_value_unsafe(&value, None),
            Err(_) => true,
        };
    }
    let bytes = text.as_bytes();
    if bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\\' && bytes.get(index + 1) != Some(&b'"'))
    {
        return true;
    }
    // The folder depth of the importer (pnpm) or workspace (Berry) block a line is in; paths
    // there are relative to it.
    let (mut base, mut in_importers, mut importer_indent) = (0usize, false, None::<usize>);
    for line in text.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent == 0 {
            base = 0;
            in_importers = name == "pnpm-lock.yaml" && trimmed == "importers:";
            importer_indent = None;
            if name == "yarn.lock" {
                match workspace_depth(&percent_decoded(trimmed)) {
                    Some(None) => return true,
                    Some(Some(depth)) => base = depth,
                    None => {}
                }
            }
        } else if in_importers && *importer_indent.get_or_insert(indent) == indent {
            let key = trimmed.trim_matches(['\'', '"', ':']);
            if !trimmed.ends_with(':') || path_leaves_checkout(key, 0) {
                return true;
            }
            base = key
                .split('/')
                .filter(|segment| !matches!(*segment, "" | "."))
                .count();
        }
        let depth = match workspace_depth(&percent_decoded(line)) {
            Some(None) => return true,
            Some(Some(depth)) if line.contains("::locator=") => depth,
            _ => base,
        };
        if lockfile_line_unsafe(line, depth) {
            return true;
        }
    }
    false
}

/// Whether one lockfile line (or JSON string) names a source [`unsafe_dependency_source`]
/// refuses, or a bare path after a `directory` or `resolved` key that leaves the checkout.
pub(super) fn lockfile_line_unsafe(line: &str, depth: usize) -> bool {
    // A pnpm `resolution: {type: git, …}` carries the kind as a word, not a prefix.
    let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if squeezed.contains("type:git") || squeezed.contains("\"type\":\"git\"") {
        return true;
    }
    let mut path_next = false;
    for token in line
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '{' | '}' | '[' | ']' | '(' | ')')
        })
        .filter(|token| !token.is_empty())
    {
        // A trailing `:` ends a YAML key (`x@file:vendor/x:`), not the path.
        if unsafe_dependency_source(token.trim_end_matches(':'), depth, false) {
            return true;
        }
        if path_next && token != ":" {
            path_next = false;
            if bare_path_unsafe(token, depth) {
                return true;
            }
        }
        path_next = path_next || matches!(token.trim_end_matches(':'), "directory" | "resolved");
    }
    false
}

/// Whether `value` is a path with no protocol that leaves the checkout.
pub(super) fn bare_path_unsafe(value: &str, depth: usize) -> bool {
    let scheme = value.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    !scheme && path_leaves_checkout(value, depth)
}

/// npm lockfile entry fields that describe a package but never choose where it comes from:
/// a `funding` URL on GitHub, say, is not a git dependency. Every other field is checked.
pub(super) const NPM_LOCKFILE_METADATA: &[&str] = &[
    "funding",
    "license",
    "engines",
    "bin",
    "os",
    "cpu",
    "libc",
    "integrity",
    "deprecated",
    "hasInstallScript",
    "peerDependenciesMeta",
];

/// Whether a parsed npm lockfile (`package-lock.json`, `npm-shrinkwrap.json`) holds a source
/// [`unsafe_dependency_source`] refuses in any string or key, a `resolved` path (a linked
/// package) that leaves the checkout, or a `packages` location outside it.
pub(super) fn npm_lockfile_value_unsafe(value: &Value, key: Option<&str>) -> bool {
    match value {
        Value::String(text) => {
            lockfile_line_unsafe(text, 0) || (key == Some("resolved") && bare_path_unsafe(text, 0))
        }
        Value::Array(items) => items
            .iter()
            .any(|item| npm_lockfile_value_unsafe(item, None)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("git") {
                return true;
            }
            if key == Some("packages")
                && map
                    .keys()
                    .any(|location| !location.is_empty() && path_leaves_checkout(location, 0))
            {
                return true;
            }
            map.iter()
                .filter(|(name, _)| !NPM_LOCKFILE_METADATA.contains(&name.as_str()))
                .any(|(name, item)| {
                    lockfile_line_unsafe(name, 0) || npm_lockfile_value_unsafe(item, Some(name))
                })
        }
        _ => false,
    }
}

/// Whether a `pnpm-workspace.yaml` lists workspace globs that leave the checkout, or settings
/// (`overrides`, `catalog`, `catalogs`, `packageExtensions`, `patchedDependencies`) naming a
/// refused source, or is not plain YAML (anchors, aliases, tags, merge keys and anything that
/// does not parse are refused).
pub(super) fn pnpm_workspace_unsafe(text: &str) -> bool {
    use yaml_rust2::{
        parser::{Event, MarkedEventReceiver, Parser},
        scanner::Marker,
    };
    #[derive(Default)]
    struct Events(Vec<Event>);
    impl MarkedEventReceiver for Events {
        fn on_event(&mut self, event: Event, _mark: Marker) {
            self.0.push(event);
        }
    }
    enum Node {
        Scalar(String),
        Seq(Vec<Node>),
        Map(Vec<(String, Node)>),
    }
    fn node(events: &mut std::slice::Iter<Event>, depth: usize) -> Option<Node> {
        if depth > 16 {
            return None;
        }
        match events.next()? {
            Event::Scalar(value, _, 0, None) => Some(Node::Scalar(value.clone())),
            Event::SequenceStart(0, None) => {
                let mut items = Vec::new();
                while !matches!(events.as_slice().first()?, Event::SequenceEnd) {
                    items.push(node(events, depth + 1)?);
                }
                events.next();
                Some(Node::Seq(items))
            }
            Event::MappingStart(0, None) => {
                let mut entries = Vec::new();
                while !matches!(events.as_slice().first()?, Event::MappingEnd) {
                    let Node::Scalar(key) = node(events, depth + 1)? else {
                        return None;
                    };
                    if key == "<<" {
                        return None;
                    }
                    entries.push((key, node(events, depth + 1)?));
                }
                events.next();
                Some(Node::Map(entries))
            }
            _ => None,
        }
    }
    fn any_scalar(node: &Node, test: &dyn Fn(&str) -> bool) -> bool {
        match node {
            Node::Scalar(text) => test(text),
            Node::Seq(items) => items.iter().any(|item| any_scalar(item, test)),
            Node::Map(entries) => entries.iter().any(|(_, item)| any_scalar(item, test)),
        }
    }
    let mut receiver = Events::default();
    if Parser::new_from_str(text)
        .load(&mut receiver, true)
        .is_err()
    {
        return true;
    }
    let mut events = receiver.0.iter();
    let mut roots = Vec::new();
    while let Some(event) = events.next() {
        match event {
            Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentStart => {
                let Some(root) = node(&mut events, 0) else {
                    return true;
                };
                if !matches!(events.next(), Some(Event::DocumentEnd)) {
                    return true;
                }
                roots.push(root);
            }
            _ => return true,
        }
    }
    let Some(root) = roots.first().filter(|_| roots.len() == 1) else {
        return !roots.is_empty();
    };
    let Node::Map(entries) = root else {
        return true;
    };
    entries.iter().any(|(key, value)| match key.as_str() {
        "packages" => any_scalar(value, &|glob| glob_leaves_checkout(glob, 0)),
        "overrides" | "catalog" | "catalogs" | "packageExtensions" => {
            any_scalar(value, &|spec| unsafe_dependency_source(spec, 0, true))
        }
        "patchedDependencies" => any_scalar(value, &|path| path_leaves_checkout(path, 0)),
        _ => false,
    })
}

/// Whether the dependency specifier `spec` names a source that runs repository code or reads
/// outside the checkout (see [`node_dependency_source_unchecked`] for each decision). `depth` is
/// how many folders below the checkout the file holding it is. `manifest` is true for a
/// `package.json` value, where a bare path and `owner/repo` are also sources; in a lockfile they
/// are names and paths of other things. The specifier and the text after each `@` in it are
/// checked, so `npm:x@exec:./g.js` and `patch:x@exec%3A./g.js#./p.patch` are caught.
pub(super) fn unsafe_dependency_source(spec: &str, depth: usize, manifest: bool) -> bool {
    let decoded = percent_decoded(spec.trim());
    let spec = decoded.as_str();
    let prefixed = |text: &str, prefix: &str| {
        text.len() > prefix.len()
            && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    };
    let mut starts = vec![0];
    starts.extend(spec.match_indices('@').map(|(index, _)| index + 1));
    let source = |text: &str| {
        // `exec:` runs a file, git sources run `prepare`-style scripts.
        if [
            "exec:",
            "git:",
            "git+",
            "github:",
            "gitlab:",
            "bitbucket:",
            "gist:",
            "git@",
            "ssh:",
        ]
        .iter()
        .any(|prefix| prefixed(text, prefix))
        {
            return true;
        }
        for scheme in ["https://", "http://"] {
            if prefixed(text, scheme) {
                let address = &text[scheme.len()..];
                let host = address.split(['/', '#', '?']).next().unwrap_or("");
                let path = address.split(['#', '?']).next().unwrap_or("");
                return path.ends_with(".git")
                    || [
                        "github.com",
                        "codeload.github.com",
                        "gist.github.com",
                        "api.github.com",
                        "gitlab.com",
                        "bitbucket.org",
                    ]
                    .iter()
                    .any(|git_host| host.eq_ignore_ascii_case(git_host));
            }
        }
        // A Windows drive path, bare, in a lockfile or a manifest.
        if drive_path(text) {
            return true;
        }
        ["file:", "link:", "portal:"]
            .iter()
            .find(|prefix| prefixed(text, prefix))
            .is_some_and(|prefix| path_leaves_checkout(&text[prefix.len()..], depth))
    };
    if starts.iter().any(|start| source(&spec[*start..])) {
        return true;
    }
    if !manifest {
        return false;
    }
    // A bare path; `~1.2.3` is a version range, `~/x` a home folder.
    if ["./", "../", ".\\", "..\\", "/", "~/", "\\"]
        .iter()
        .any(|prefix| spec.starts_with(prefix))
        || matches!(spec, "." | ".." | "~")
        || drive_path(spec)
    {
        return path_leaves_checkout(spec, depth);
    }
    // npm reads `owner/repo` as a GitHub repository.
    let repository = spec.split('#').next().unwrap_or("");
    !repository.contains(':') && !repository.starts_with('@') && repository.contains('/')
}

/// Whether `text` starts with a Windows drive and a separator (`C:\`, `C:/`).
pub(super) fn drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

/// Whether a dependency path (without its protocol) is absolute, names a drive or home folder,
/// or leaves the checkout, counting from `depth` folders below it. Anything after `#` or `::`
/// is a selector, not part of the path.
pub(super) fn path_leaves_checkout(path: &str, depth: usize) -> bool {
    let path = path.split('#').next().unwrap_or("");
    let path = path.split("::").next().unwrap_or("");
    if path.is_empty() {
        return false;
    }
    if path.starts_with(['/', '\\', '~']) || path.contains([':', '\\']) {
        return true;
    }
    let mut depth = depth;
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(rest) => depth = rest,
                None => return true,
            },
            _ => depth += 1,
        }
    }
    false
}

/// `text` with `%XX` escapes decoded (invalid escapes stay as written).
pub(super) fn percent_decoded(text: &str) -> String {
    if !text.contains('%') {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether every token of a requirements-file line's options is one of `allowed`, each taking
/// its value inline (`--name=value`) or as the next token when it takes one. Quoted tokens are
/// refused, since pip unquotes options and a quoted token could hide another option.
pub(super) fn allowed_options(tokens: &[&str], allowed: &[(&str, bool)]) -> bool {
    let quoted = |token: &str| token.contains(['"', '\'']);
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        if quoted(token) {
            return false;
        }
        let (name, inline) = match token.split_once('=') {
            Some((name, _)) => (name, true),
            None => (*token, false),
        };
        let Some(&(_, valued)) = allowed.iter().find(|(option, _)| *option == name) else {
            return false;
        };
        if valued && !inline {
            match tokens.next() {
                Some(value) if !value.starts_with('-') && !quoted(value) => {}
                _ => return false,
            }
        } else if !valued && inline {
            return false;
        }
    }
    true
}
