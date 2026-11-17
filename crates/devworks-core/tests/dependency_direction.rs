//! Structural checks on the core's dependency direction (code review F-041). They read the
//! source files, so they run in normal CI with the rest of this crate's tests.
//!
//! * The gateway, authority, settings, ports, providers and kernel never import the services.
//! * The service helpers live in `services::kernel` and the Git client in `services::git`, and
//!   neither sessions nor indexing exports them.
//! * No service calls `std::fs`, `std::env`, `Instant::now` or `SystemTime::now` outside the
//!   allowlist below, which names each remaining file and the reason.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

fn relative(path: &Path) -> String {
    path.strip_prefix(src())
        .expect("under src")
        .to_string_lossy()
        .replace('\\', "/")
}

/// The code lines of a file with comments and test-only items removed, as `(line number,
/// text)`. A test-only item is one under a `#[cfg(...)]` that names `test`; it ends at its
/// closing brace, or at its `;` when it has no body.
fn production_lines(path: &Path) -> Vec<(usize, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((number, line)) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        let test_gate = trimmed.starts_with("#[cfg(")
            && trimmed
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|word| word == "test");
        if test_gate {
            let mut depth = 0i32;
            let mut opened = false;
            for (_, item) in lines.by_ref() {
                let code = item.split("//").next().unwrap_or("");
                if code.trim_start().starts_with("#[") && !opened {
                    continue;
                }
                depth += code.matches('{').count() as i32;
                opened |= code.contains('{');
                depth -= code.matches('}').count() as i32;
                if (opened && depth <= 0) || (!opened && code.contains(';')) {
                    break;
                }
            }
            continue;
        }
        out.push((number + 1, line.split("//").next().unwrap_or("").to_owned()));
    }
    out
}

fn files_under(parts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for part in parts {
        let path = src().join(part);
        if path.is_dir() {
            rust_files(&path, &mut out);
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn lower_layers_do_not_import_services() {
    let mut found = Vec::new();
    for path in files_under(&[
        "gateway.rs",
        "authority",
        "settings",
        "ports",
        "providers",
        "kernel",
    ]) {
        for (number, line) in production_lines(&path) {
            if line.contains("crate::services") {
                found.push(format!("{}:{number}: {}", relative(&path), line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "these modules must not name `crate::services`; move what they need to `crate::kernel` \
         or the owning port:\n{}",
        found.join("\n")
    );
}

#[test]
fn service_helpers_and_git_client_have_one_neutral_home() {
    let services = src().join("services");
    assert!(services.join("kernel.rs").is_file());
    assert!(services.join("git/client.rs").is_file());
    assert!(services.join("git/helper.rs").is_file());
    assert!(
        !services.join("indexing/git.rs").exists() && !services.join("indexing/helper.rs").exists(),
        "the Git client and its helper runner belong to services/git"
    );
    let sessions = fs::read_to_string(services.join("sessions/mod.rs")).expect("sessions");
    for exported in ["pub use super::kernel", "pub(crate) use super::kernel"] {
        assert!(
            !sessions.contains(exported),
            "sessions must not re-export the service kernel"
        );
    }
    for definition in ["fn not_found(", "fn query_fault(", "fn decided(", "fn rev("] {
        assert!(
            !sessions.contains(definition),
            "{definition} is a kernel helper, not part of sessions"
        );
    }
    let indexing = fs::read_to_string(services.join("indexing/mod.rs")).expect("indexing");
    assert!(
        !indexing.contains("pub mod git;") && !indexing.contains("pub mod helper;"),
        "indexing must not export the Git client"
    );
}

/// Services files that still call the standard library's file system, environment or clocks,
/// with the number of lines that do and the reason each is not routed through a port.
///
/// `Clock` is a wall clock in milliseconds. Every `Instant::now` below measures a monotonic
/// deadline or elapsed time around blocking work, which a wall clock cannot do (it can step
/// backwards), so those stay on the standard library until a monotonic clock port exists.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "services/agents.rs",
        3,
        "monotonic deadline of a bounded wait for new messages",
    ),
    (
        "services/browser/cdp.rs",
        4,
        "monotonic deadline of one Chrome DevTools exchange",
    ),
    (
        "services/browser/egress.rs",
        1,
        "monotonic window of the per-page request budget",
    ),
    (
        "services/browser/live.rs",
        6,
        "monotonic navigation and settle deadlines; one SystemTime read only seeds the \
         unguessable token with extra entropy, never a stored time",
    ),
    (
        "services/containers/build_context.rs",
        4,
        "reads the build-context directory it digests, which has no port: it is the user's \
         own tree, read-only, never written",
    ),
    (
        "services/containers/runner.rs",
        5,
        "monotonic deadlines of the container command runner",
    ),
    (
        "services/containers/shutdown.rs",
        6,
        "monotonic deadline of the bounded container shutdown",
    ),
    (
        "services/diagnostics/mod.rs",
        3,
        "monotonic elapsed time and export-claim wait deadline",
    ),
    (
        "services/git/helper.rs",
        1,
        "monotonic elapsed time of one helper process",
    ),
    (
        "services/git/mod.rs",
        1,
        "monotonic elapsed time of a Git use case",
    ),
    (
        "services/graph/budget.rs",
        1,
        "monotonic start of a query's time budget",
    ),
    (
        "services/graph/service.rs",
        1,
        "monotonic start of a query's time budget",
    ),
    (
        "services/mcp/content.rs",
        3,
        "monotonic deadline of an MCP content read",
    ),
    (
        "services/process.rs",
        4,
        "monotonic timers of the process owner's bounded waits",
    ),
    (
        "services/process/executables.rs",
        3,
        "resolves a program on the host PATH (an environment read and a file-metadata \
         probe); the PATH source is injectable (`from_settings_on`) and tests use it",
    ),
    (
        "services/process/tools.rs",
        13,
        "toolchain detection reads the project's manifests and the host PATH, RUSTUP_HOME and \
         HOME to build a tool's environment, and creates the scratch directory it runs in; \
         each read is bounded and none writes outside that directory; plus monotonic \
         deadlines of a tool run",
    ),
    (
        "services/watch/mod.rs",
        5,
        "reads the tail of a watched log file with a no-follow, non-blocking open (the file \
         is the user's, outside every port's root); plus monotonic pass deadlines",
    ),
    ("services/web/fetch.rs", 2, "monotonic deadline of a fetch"),
    (
        "services/web/mod.rs",
        2,
        "monotonic deadline of a web request",
    ),
];

fn counted(path: &Path) -> usize {
    const PATTERNS: [&str; 4] = ["std::fs", "std::env", "Instant::now", "SystemTime::now"];
    production_lines(path)
        .iter()
        .filter(|(_, line)| PATTERNS.iter().any(|p| line.contains(p)))
        .count()
}

#[test]
fn services_reach_files_environment_and_clocks_only_where_listed() {
    let allowed: BTreeMap<&str, usize> = ALLOWED.iter().map(|(f, n, _)| (*f, *n)).collect();
    assert!(
        ALLOWED.iter().all(|(_, _, reason)| reason.len() > 20),
        "every allowlist entry states its reason"
    );
    let mut problems = Vec::new();
    for path in files_under(&["services"]) {
        let name = relative(&path);
        let found = counted(&path);
        match allowed.get(name.as_str()) {
            None if found > 0 => problems.push(format!(
                "{name}: {found} direct file system, environment or clock call(s); route them \
                 through a port or add the file to ALLOWED with a reason"
            )),
            Some(&expected) if expected != found => problems.push(format!(
                "{name}: ALLOWED lists {expected} but the file has {found}; update the entry"
            )),
            _ => {}
        }
    }
    for name in allowed.keys() {
        if !src().join(name).is_file() {
            problems.push(format!("{name}: ALLOWED names a file that does not exist"));
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
