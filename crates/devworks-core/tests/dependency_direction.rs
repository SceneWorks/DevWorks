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

/// Rust source with comments removed and line breaks kept, so line numbers still match.
/// It skips line comments, nested block comments, string literals (so `"http://x"` is not a
/// comment) and raw strings, and tells a char literal from a lifetime.
fn ch(c: &[char], at: usize) -> char {
    c.get(at).copied().unwrap_or('\0')
}

fn strip_comments(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '/' if ch(&c, i + 1) == '/' => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if ch(&c, i + 1) == '*' => {
                let mut depth = 0;
                while i < c.len() {
                    if c[i] == '/' && ch(&c, i + 1) == '*' {
                        depth += 1;
                        i += 2;
                    } else if c[i] == '*' && ch(&c, i + 1) == '/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if c[i] == '\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                }
                out.push(' ');
            }
            'r' if (ch(&c, i + 1) == '"' || ch(&c, i + 1) == '#')
                && !out
                    .chars()
                    .last()
                    .is_some_and(|p| p.is_alphanumeric() || p == '_') =>
            {
                let hashes = c[i + 1..].iter().take_while(|&&x| x == '#').count();
                if ch(&c, i + 1 + hashes) != '"' {
                    out.push('r');
                    i += 1;
                    continue;
                }
                let close: Vec<char> = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let start = i;
                i += 2 + hashes;
                while i < c.len() && c[i..].get(..close.len()) != Some(&close[..]) {
                    i += 1;
                }
                i = (i + close.len()).min(c.len());
                out.extend(&c[start..i]);
            }
            '"' => {
                let start = i;
                i += 1;
                while i < c.len() && c[i] != '"' {
                    i += if c[i] == '\\' { 2 } else { 1 };
                }
                i = (i + 1).min(c.len());
                out.extend(&c[start..i]);
            }
            '\'' if ch(&c, i + 1) == '\\' || ch(&c, i + 2) == '\'' => {
                let start = i;
                i += 2;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
                i = (i + 1).min(c.len());
                out.extend(&c[start..i]);
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

/// A `#[cfg(test)]` or `#[cfg(all(test, ...))]` gate. `not(test)` and feature gates are
/// production code.
fn is_test_gate(trimmed: &str) -> bool {
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.starts_with("#[cfg(test)]") || compact.starts_with("#[cfg(all(test,")
}

/// The production code lines of source text with comments and test-only items removed, as
/// `(line number, text)`. A test-only item is one under a test gate; it ends at its closing
/// brace, or at its `;` when it has no body.
fn production_text(text: &str) -> Vec<(usize, String)> {
    let stripped = strip_comments(text);
    // A file gated as a whole (`#![cfg(test)]`) holds no production code.
    if stripped
        .lines()
        .any(|l| l.split_whitespace().collect::<String>() == "#![cfg(test)]")
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut lines = stripped.lines().enumerate();
    while let Some((number, line)) = lines.next() {
        if is_test_gate(line.trim()) {
            let mut depth = 0i32;
            let mut opened = false;
            for (_, item) in lines.by_ref() {
                if item.trim_start().starts_with("#[") && !opened {
                    continue;
                }
                depth += item.matches('{').count() as i32;
                opened |= item.contains('{');
                depth -= item.matches('}').count() as i32;
                if (opened && depth <= 0) || (!opened && item.contains(';')) {
                    break;
                }
            }
            continue;
        }
        out.push((number + 1, line.to_owned()));
    }
    out
}

fn production_lines(path: &Path) -> Vec<(usize, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    production_text(&text)
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offsets of `word` as a whole identifier in `text`.
fn word_offsets(text: &str, word: &str) -> Vec<usize> {
    text.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            !text[..at].chars().next_back().is_some_and(is_ident)
                && !text[at + word.len()..].chars().next().is_some_and(is_ident)
        })
        .collect()
}

/// Line numbers (1-based) where production text reaches the `services` module: a
/// `services::` path segment anywhere, or `services` inside a `crate::{ ... }` group, on one
/// line or several.
fn services_references(text: &str) -> Vec<usize> {
    let kept = production_text(text);
    let joined: String = kept
        .iter()
        .map(|(_, l)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut hits: Vec<usize> = Vec::new();
    let mut base = 0;
    let mut starts = Vec::new();
    for (number, line) in &kept {
        starts.push((base, *number));
        base += line.len() + 1;
    }
    let line_of = |offset: usize| {
        starts
            .iter()
            .rev()
            .find(|(start, _)| *start <= offset)
            .map_or(0, |(_, n)| *n)
    };
    for at in word_offsets(&joined, "services") {
        if joined[at + "services".len()..]
            .trim_start()
            .starts_with("::")
        {
            hits.push(line_of(at));
        }
    }
    for at in word_offsets(&joined, "crate") {
        let rest = &joined[at + "crate".len()..];
        let Some(group) = rest.trim_start().strip_prefix("::").map(str::trim_start) else {
            continue;
        };
        if !group.starts_with('{') {
            continue;
        }
        let mut depth = 0;
        let mut end = group.len();
        for (k, ch) in group.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = k;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !word_offsets(&group[..end], "services").is_empty() {
            hits.push(line_of(at));
        }
    }
    hits.sort_unstable();
    hits.dedup();
    hits
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
        let text = fs::read_to_string(&path).expect("read source");
        for number in services_references(&text) {
            found.push(format!("{}:{number}", relative(&path)));
        }
    }
    assert!(
        found.is_empty(),
        "these modules must not reach the `services` module; move what they need to `crate::kernel` \
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

/// Services files that still call the standard library's file system, environment or clocks.
/// Each entry gives the number of lines per kind (`KINDS` order: `std::fs`, `std::env`,
/// `Instant::now`, `SystemTime::now`) so a swap between kinds is caught, and the reason the
/// call is not routed through a port.
///
/// No monotonic clock port exists; `Clock` is wall-clock ms. Every `Instant::now` below
/// measures a monotonic deadline or elapsed time around blocking work, which a wall clock
/// cannot do (it can step backwards), so those stay on the standard library.
const ALLOWED: &[(&str, [usize; 4], &str)] = &[
    (
        "services/agents.rs",
        [0, 0, 2, 0],
        "monotonic deadline of a bounded wait for new messages",
    ),
    (
        "services/browser/cdp.rs",
        [0, 0, 4, 0],
        "monotonic deadline of one Chrome DevTools exchange",
    ),
    (
        "services/browser/egress.rs",
        [0, 0, 1, 0],
        "monotonic window of the per-page request budget",
    ),
    (
        "services/browser/live.rs",
        [0, 0, 5, 1],
        "monotonic navigation and settle deadlines; one SystemTime read only seeds the \
         unguessable token with extra entropy, never a stored time",
    ),
    (
        "services/containers/build_context.rs",
        [4, 0, 0, 0],
        "reads the build-context directory it digests, which has no port: it is the user's \
         own tree, read-only, never written",
    ),
    (
        "services/containers/runner.rs",
        [0, 0, 2, 0],
        "monotonic deadlines of the container command runner",
    ),
    (
        "services/containers/shutdown.rs",
        [0, 0, 6, 0],
        "monotonic deadline of the bounded container shutdown",
    ),
    (
        "services/database/mutation.rs",
        [0, 0, 1, 0],
        "monotonic timeout for a guarded native database mutation",
    ),
    (
        "services/database/read.rs",
        [0, 0, 1, 0],
        "monotonic timeout for a bounded native database read",
    ),
    (
        "services/diagnostics/mod.rs",
        [0, 0, 3, 0],
        "monotonic elapsed time and export-claim wait deadline",
    ),
    (
        "services/documents/extraction.rs",
        [0, 0, 1, 0],
        "monotonic deadline check of a bounded document extraction",
    ),
    (
        "services/documents/indexing/routed_vision.rs",
        [0, 0, 2, 0],
        "monotonic deadlines of document vision rendering and routed vision reading",
    ),
    (
        "services/documents/indexing.rs",
        [0, 0, 1, 0],
        "monotonic deadline of a document's direct text extraction",
    ),
    (
        "services/git/helper.rs",
        [0, 0, 1, 0],
        "monotonic elapsed time of one helper process",
    ),
    (
        "services/git/mod.rs",
        [0, 0, 1, 0],
        "monotonic elapsed time of a Git use case",
    ),
    (
        "services/graph/budget.rs",
        [0, 0, 1, 0],
        "monotonic start of a query's time budget",
    ),
    (
        "services/graph/repository_analysis/deterministic.rs",
        [0, 0, 1, 0],
        "monotonic start of the deterministic analysis phase budget",
    ),
    (
        "services/graph/repository_analysis/paid.rs",
        [0, 0, 1, 0],
        "monotonic deadline of bounded analysis prompt preparation",
    ),
    (
        "services/graph/repository_analysis/security/advisories.rs",
        [0, 0, 2, 0],
        "monotonic bounds for advisory response reads and individual requests",
    ),
    (
        "services/graph/repository_analysis/vitals/report.rs",
        [0, 0, 1, 0],
        "monotonic deadline shared by all Git history reads in one report",
    ),
    (
        "services/graph/service.rs",
        [0, 0, 1, 0],
        "monotonic start of a query's time budget",
    ),
    (
        "services/indexing/assigned.rs",
        [0, 0, 1, 0],
        "monotonic time limit for an assigned graph read waiting on its overlay",
    ),
    (
        "services/mcp/content.rs",
        [0, 0, 3, 0],
        "monotonic deadline of an MCP content read",
    ),
    (
        "services/process/executables.rs",
        [1, 2, 0, 0],
        "resolves a program on the host PATH (an environment read and a file-metadata \
         probe); the PATH source is injectable (`from_settings_on`) and tests use it",
    ),
    (
        "services/process/tool_modules/execute.rs",
        [3, 3, 2, 0],
        "toolchain detection reads the project's manifests and the host PATH, RUSTUP_HOME and \
         HOME to build a tool's environment, and creates the scratch directory it runs in; \
         each read is bounded and none writes outside that directory; plus monotonic \
         deadlines of a tool run",
    ),
    (
        "services/process/tool_modules/install_safety.rs",
        [13, 0, 0, 0],
        "the script-free install safety checks read repository manifests and lockfiles \
         (package.json, .yarnrc.yml, lockfiles) and the ancestor directories' rc files, which \
         are filesystem reads of the checked-out repository with no port to go through; each \
         read is bounded and none writes",
    ),
    (
        "services/process.rs",
        [0, 0, 8, 0],
        "monotonic timers of the process owner's bounded waits",
    ),
    (
        "services/watch/file_scan.rs",
        [3, 0, 0, 0],
        "bounded reads and identity checks for a user-selected regular file outside the \
         port roots, opened without following symlinks or blocking on a FIFO on Unix",
    ),
    (
        "services/watch/mod.rs",
        [0, 0, 2, 0],
        "monotonic deadlines for watcher passes",
    ),
    (
        "services/web/fetch.rs",
        [0, 0, 2, 0],
        "monotonic deadline of a fetch",
    ),
    (
        "services/web/mod.rs",
        [0, 0, 2, 0],
        "monotonic deadline of a web request",
    ),
];

/// Lines that call the standard library, per kind: file system, environment,
/// `Instant::now`, `SystemTime::now`.
const KINDS: [&str; 4] = ["std::fs", "std::env", "Instant::now", "SystemTime::now"];

fn counted_text(text: &str) -> [usize; 4] {
    let mut counts = [0; 4];
    for (_, line) in production_text(text) {
        for (count, kind) in counts.iter_mut().zip(KINDS) {
            if line.contains(kind) {
                *count += 1;
            }
        }
    }
    counts
}

#[test]
fn services_reach_files_environment_and_clocks_only_where_listed() {
    let allowed: BTreeMap<&str, [usize; 4]> = ALLOWED.iter().map(|(f, n, _)| (*f, *n)).collect();
    assert!(
        ALLOWED.iter().all(|(_, _, reason)| reason.len() > 20),
        "every allowlist entry states its reason"
    );
    let mut problems = Vec::new();
    for path in files_under(&["services"]) {
        let name = relative(&path);
        let text = fs::read_to_string(&path).expect("read source");
        let found = counted_text(&text);
        match allowed.get(name.as_str()) {
            None if found != [0; 4] => problems.push(format!(
                "{name}: direct calls {found:?} ({KINDS:?}); route them through a port or add \
                 the file to ALLOWED with a reason"
            )),
            Some(&expected) if expected != found => problems.push(format!(
                "{name}: ALLOWED lists {expected:?} but the file has {found:?} ({KINDS:?}); \
                 update the entry"
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

/// Only `services/projects/purge.rs` may name the purge confirmation trait or call
/// `PurgePermit::issue`, so no other module can mint the destructive permit.
#[test]
fn only_the_purge_owner_can_issue_a_purge_permit() {
    let mut found = Vec::new();
    for path in files_under(&[""]) {
        let name = relative(&path);
        if name == "services/projects/purge.rs" || name == "ports/project_purge.rs" {
            continue;
        }
        for (number, line) in production_lines(&path) {
            if line.contains("PurgeConfirmation") || line.contains("PurgePermit::issue") {
                found.push(format!("{name}:{number}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "only purge.rs may issue a PurgePermit:\n{}",
        found.join("\n")
    );
    let owner = fs::read_to_string(src().join("services/projects/purge.rs")).expect("purge");
    assert!(
        owner.contains("\nstruct ConfirmationToken(());"),
        "the confirmation token stays private to purge.rs with a private field"
    );
}

#[test]
fn scanner_handles_braced_multiline_string_and_block_comment_cases() {
    let hits = services_references;
    assert_eq!(hits("use crate::services::x;"), [1]);
    assert_eq!(hits("use crate::{services::x, other};"), [1]);
    assert_eq!(hits("use crate::{services};"), [1]);
    assert_eq!(
        hits("use crate::{\n    other,\n    services::x,\n};"),
        [1, 3]
    );
    assert_eq!(
        hits("fn f() {\n    let _ = x::services\n        ::y();\n}"),
        [2]
    );
    // Not references.
    assert!(hits("use crate::{other, kernel::x};").is_empty());
    assert!(hits("let m = my_services::x; let s = services_count;").is_empty());
    // A comment that mentions it, line or block, is not code; an http:// string must not
    // hide the code after it.
    assert!(hits("// use crate::services::x;").is_empty());
    assert!(hits("/* use crate::services::x;\n   more */ fn f() {}").is_empty());
    assert!(hits("/* a /* nested */ use crate::services::x; */").is_empty());
    assert_eq!(hits("let u = \"http://x\"; use crate::services::x;"), [1]);
    assert_eq!(hits("let r = r#\"//\"#; use crate::services::x;"), [1]);
    assert_eq!(hits("let c = '\"'; use crate::services::x;"), [1]);
    assert_eq!(hits("fn f<'a>(x: &'a str) { crate::services::y(); }"), [1]);
}

#[test]
fn scanner_treats_only_test_gates_as_test_only() {
    let hits = services_references;
    assert!(hits("#[cfg(test)]\nmod t {\n    use crate::services::x;\n}").is_empty());
    assert!(hits("#[cfg(all(test, unix))]\nmod t {\n    use crate::services::x;\n}").is_empty());
    assert!(hits("#[cfg(test)]\nuse crate::services::x;\nfn f() {}").is_empty());
    assert_eq!(hits("#[cfg(not(test))]\nuse crate::services::x;"), [2]);
    assert_eq!(
        hits("#[cfg(feature = \"test\")]\nuse crate::services::x;"),
        [2]
    );
    assert_eq!(
        hits("#[cfg(any(test, unix))]\nuse crate::services::x;"),
        [2]
    );
    // Code after a test-only module is production again.
    assert_eq!(
        hits("#[cfg(test)]\nmod t {\n}\nuse crate::services::x;"),
        [4]
    );
    assert_eq!(
        counted_text("#[cfg(not(test))]\nfn f() { std::env::var(\"A\"); }"),
        [0, 1, 0, 0]
    );
    assert_eq!(
        counted_text("#[cfg(test)]\nfn f() { std::env::var(\"A\"); }"),
        [0; 4]
    );
}
