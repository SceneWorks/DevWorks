//! `dev-host guard`: stops new hardcoded machine paths (requirement E7) and checks the agent
//! docs (requirement E12).
//!
//! # Path rules
//!
//! `--diff <base>` checks the lines added since the merge base with `<base>` (tracked files,
//! working tree included); `--all` checks every line of every tracked file. Both look only at
//! files matching the path globs ([`DEFAULT_PATHS`] unless `--path` is given) and skip docs
//! (`*.md`, `*.mdx`, `*.markdown`). A line fails with one reason, the first that applies:
//!
//! 1. user app data: `~/SceneWorks`, `$HOME/SceneWorks`, `/Users/<name>/SceneWorks`,
//!    `Library/Application Support/SceneWorks`, or a home lookup joined with `"SceneWorks"`;
//! 2. an absolute-path cargo launch: `…/cargo`, `…\cargo.exe` (e.g. `~/.cargo/bin/cargo`), or a
//!    join of `".cargo"`, `"bin"` and `"cargo"`;
//! 3. `CARGO_TARGET_DIR` (or `--target-dir`) set to a value that does not mention
//!    `DEV_HOST_ROOT`, `dev-host` or an allowed prefix;
//! 4. a `<repo>/target` assumption: a `target` path component (`./target/`, `$X/target`,
//!    `target/release`, `target\debug`) or a path join with `"target"`;
//! 5. a machine path: `/Users/`, `/Volumes/`, `/tmp`, `/private/tmp`, `~/`, `$HOME/`,
//!    `${HOME}`, `%USERPROFILE%`, `%PUBLIC%`, `$env:USERPROFILE`, `$env:PUBLIC`,
//!    `Path.home()`, `homedir()`, `expanduser(`, or a drive path such as `C:\` or `E:/`.
//!
//! Exceptions: a line carrying `dev-host: allow-path <reason>` (the reason is required), a
//! comment-only line (`#`, `//`, `/*`, `*`, `<!--`, `::`, `REM`), and paths under an allowed
//! prefix: the system tool folders in [`SYSTEM_PREFIXES`] plus each `--allow-prefix`.
//!
//! # Doc rule
//!
//! `--docs` reads `AGENTS.md` (required) and `FEATURE_DEVELOPMENT.md` (when present). Each may
//! carry one section between `<!-- dev-host -->` and `<!-- /dev-host -->`; AGENTS.md must.
//! Across those sections the text must contain every item in [`doc_items`]: `DEV_HOST_ROOT`;
//! the layout folders `build/`, `t/`, `cache/`, `weights/`, `campaigns/`, `locks/`;
//! `dev-host env`; `dev-host: allow-path`; and the search rule, written as "never search"
//! together with `$HOME` (case-insensitive).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The exception comment.
pub const ALLOW_MARK: &str = "dev-host: allow-path";

/// Files the path rules check unless `--path` is given.
pub const DEFAULT_PATHS: &[&str] = &[".github/**", "**/scripts/**", "**/tools/**"];

/// System tool folders whose paths are never state.
pub const SYSTEM_PREFIXES: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/bin",
    "/usr/local/bin",
    "/bin",
    r"C:\Windows",
    r"C:\Program Files",
];

pub const DOC_START: &str = "<!-- dev-host -->";
pub const DOC_END: &str = "<!-- /dev-host -->";

/// One failing line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: usize,
    pub reason: &'static str,
    pub text: String,
}

impl Finding {
    pub fn render(&self) -> String {
        let mut text: String = self.text.trim().chars().take(200).collect();
        if self.text.trim().chars().count() > 200 {
            text.push('…');
        }
        format!("{}:{}: {}\n    {text}\n", self.file, self.line, self.reason)
    }
}

pub const R_ALLOW_REASON: &str = "`dev-host: allow-path` needs a reason after it";
pub const R_APP_DATA: &str = "references user app data (~/SceneWorks or ~/Library/Application \
                              Support/SceneWorks), which runners and agents never touch (E10)";
pub const R_ABS_CARGO: &str = "launches cargo by absolute path; run `cargo` from PATH so the \
                               dev-host environment applies (E4)";
pub const R_TARGET_DIR: &str = "sets CARGO_TARGET_DIR (or --target-dir) to a path not under \
                                DEV_HOST_ROOT; let `dev-host env` set it (E4)";
pub const R_REPO_TARGET: &str = "assumes build output in <repo>/target; read \
                                 `cargo metadata` .target_directory instead (E4)";
pub const R_MACHINE_PATH: &str = "hardcodes a machine path outside DEV_HOST_ROOT; derive it from \
                                  DEV_HOST_ROOT or add `dev-host: allow-path <reason>` (E1)";

/// Allowed path prefixes.
#[derive(Debug, Clone)]
pub struct Rules {
    prefixes: Vec<String>,
}

impl Default for Rules {
    fn default() -> Self {
        Self::new(&[])
    }
}

impl Rules {
    /// The system prefixes plus `extra`.
    pub fn new(extra: &[String]) -> Self {
        let mut prefixes: Vec<String> = SYSTEM_PREFIXES.iter().map(|p| p.to_string()).collect();
        prefixes.extend(extra.iter().cloned());
        Self { prefixes }
    }

    /// Whether `text` (from the start of a path onward) begins with an allowed prefix that
    /// ends at a folder boundary.
    fn allowed(&self, text: &str) -> bool {
        self.prefixes.iter().any(|prefix| {
            let windows = prefix.as_bytes().get(1) == Some(&b':');
            // Windows prefixes compare case-insensitively, with `/` and escaped `\\` read as `\`.
            let windows_form = |s: &str| {
                s.to_ascii_lowercase()
                    .replace("\\\\", "\\")
                    .replace('/', "\\")
            };
            let (t, p) = if windows {
                (windows_form(text), windows_form(prefix))
            } else {
                (text.to_owned(), prefix.clone())
            };
            let p = p.trim_end_matches(['/', '\\']);
            t.starts_with(p)
                && t[p.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| c == '/' || c == '\\' || c == ':' || ends_token(c))
        })
    }
}

fn prev_char(line: &str, i: usize) -> Option<char> {
    line[..i].chars().next_back()
}

fn next_char(line: &str, i: usize) -> Option<char> {
    line[i..].chars().next()
}

/// Characters that end a path token.
fn ends_token(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}' || "\"'`=(),[];|<>".contains(c)
}

/// The start of a path may follow these (or the line start).
fn starts_path(prev: Option<char>) -> bool {
    prev.is_none_or(|c| ends_token(c) || c == ':' || c == '{')
}

fn token_start(line: &str, i: usize) -> usize {
    line[..i]
        .char_indices()
        .rev()
        .find(|(_, c)| ends_token(*c))
        .map_or(0, |(j, c)| j + c.len_utf8())
}

/// The line from the start of the token around byte index `i`.
fn from_token(line: &str, i: usize) -> &str {
    &line[token_start(line, i)..]
}

/// The whitespace/quote-delimited token around byte index `i`.
fn token(line: &str, i: usize) -> &str {
    let start = token_start(line, i);
    let end = line[i..]
        .char_indices()
        .find(|(_, c)| ends_token(*c))
        .map_or(line.len(), |(j, _)| i + j);
    &line[start..end]
}

fn comment_only(line: &str) -> bool {
    let t = line.trim_start();
    (t.starts_with('#') && !t.starts_with("#["))
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with("<!--")
        || t.starts_with("::")
        || t.starts_with("REM ")
        || t.starts_with("rem ")
        || t == "*"
        || t.starts_with("* ")
        || t.starts_with("*/")
}

/// Checks one line; `None` passes.
pub fn check_line(line: &str, rules: &Rules) -> Option<&'static str> {
    if let Some(at) = line.find(ALLOW_MARK) {
        let reason = line[at + ALLOW_MARK.len()..]
            .trim()
            .trim_end_matches("-->")
            .trim_end_matches("*/")
            .trim();
        return (reason.is_empty() || !reason.chars().any(char::is_alphanumeric))
            .then_some(R_ALLOW_REASON);
    }
    if comment_only(line) {
        return None;
    }
    if app_data(line) {
        Some(R_APP_DATA)
    } else if abs_cargo(line, rules) {
        Some(R_ABS_CARGO)
    } else if target_dir_set(line, rules) {
        Some(R_TARGET_DIR)
    } else if repo_target(line) {
        Some(R_REPO_TARGET)
    } else if machine_path(line, rules) {
        Some(R_MACHINE_PATH)
    } else {
        None
    }
}

fn app_data(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    // The folder itself, not a sibling such as `~/SceneWorks-campaign`.
    let folder_ends = |rest: &str| {
        rest.strip_prefix("sceneworks").is_some_and(|after| {
            after
                .chars()
                .next()
                .is_none_or(|c| c == '/' || c == '\\' || ends_token(c))
        })
    };
    for prefix in [
        "~/",
        "$home/",
        "${home}/",
        "application support/",
        "home() / \"",
        "home() / '",
        "home()/\"",
        "home()/'",
    ] {
        if l.match_indices(prefix)
            .any(|(i, _)| folder_ends(&l[i + prefix.len()..]))
        {
            return true;
        }
    }
    // `/Users/<name>/SceneWorks`; `/Users/Shared` is no one's home.
    l.match_indices("/users/").any(|(i, _)| {
        let rest = &l[i + "/users/".len()..];
        if rest.starts_with("shared/") {
            return false;
        }
        rest.find('/')
            .is_some_and(|slash| folder_ends(&rest[slash + 1..]))
    })
}

fn abs_cargo(line: &str, rules: &Rules) -> bool {
    let by_path = line.match_indices("cargo").any(|(i, _)| {
        let prev = prev_char(line, i);
        let rest = &line[i + "cargo".len()..];
        let ends = rest
            .chars()
            .next()
            .is_none_or(|c| ends_token(c) || "&".contains(c))
            || rest.starts_with(".exe");
        // The separator must follow a folder name (`bin/cargo`, `$CARGO_HOME/bin/cargo`), not
        // open a token, as in the regex `/cargo build/`.
        let after_folder = line[..i]
            .char_indices()
            .next_back()
            .and_then(|(j, _)| prev_char(line, j))
            .is_some_and(|c| !ends_token(c));
        matches!(prev, Some('/' | '\\'))
            && after_folder
            && ends
            && !rules.allowed(from_token(line, i))
    });
    let quoted =
        |word: &str| line.contains(&format!("\"{word}\"")) || line.contains(&format!("'{word}'"));
    by_path
        || (line.contains(".cargo") && quoted("bin") && (quoted("cargo") || quoted("cargo.exe")))
}

fn mentions_root(value: &str, rules: &Rules) -> bool {
    value.contains("DEV_HOST_ROOT")
        || value.contains("dev-host")
        || value
            .split(ends_token)
            .any(|t| !t.is_empty() && rules.allowed(t.trim_start_matches(['$', '{'])))
}

fn target_dir_set(line: &str, rules: &Rules) -> bool {
    const NAME: &str = "CARGO_TARGET_DIR";
    const SETTERS: &[&str] = &[
        "env",
        "setdefault",
        "set_var",
        "putenv",
        "setenv",
        "insert",
        "SetEnvironmentVariable",
    ];
    let env_set = line.match_indices(NAME).any(|(i, _)| {
        let before = &line[..i];
        let after = &line[i + NAME.len()..];
        if after
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }
        if before.ends_with('$') || before.ends_with("${") || before.ends_with('%') {
            return false;
        }
        let rest = after.trim_start_matches(['"', '\'', ']']).trim_start();
        let value = if let Some(v) = rest.strip_prefix('=') {
            if v.starts_with('=') {
                return false;
            }
            v
        } else if let Some(v) = rest.strip_prefix(':') {
            if v.starts_with(['-', '=', '?', '+']) {
                return false;
            }
            v
        } else if let Some(v) = rest.strip_prefix(',') {
            let call = before.trim_end_matches(['"', '\'']).trim_end();
            let Some(call) = call.strip_suffix('(') else {
                return false;
            };
            if !SETTERS.iter().any(|s| call.ends_with(s)) {
                return false;
            }
            v
        } else {
            return false;
        };
        let value = value.trim();
        !value.is_empty() && !mentions_root(value, rules)
    });
    let flag_set = line.match_indices("--target-dir").any(|(i, _)| {
        let rest = &line[i + "--target-dir".len()..];
        let Some(value) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(' ')) else {
            return false;
        };
        let value = value.trim_start();
        let value = value.split(char::is_whitespace).next().unwrap_or("");
        !value.is_empty() && !mentions_root(value, rules)
    });
    env_set || flag_set
}

/// Folder names cargo writes directly under its target folder.
const CARGO_OUTPUT_DIRS: &[&str] = &[
    "release",
    "debug",
    "doc",
    "tmp",
    "package",
    "build",
    "deps",
    "incremental",
    "examples",
    "nextest",
    "criterion",
];

/// Words a quoted string needs before a slash list inside it counts as prose.
const PROSE_WORDS: usize = 5;

/// Prose such as "operation/target/stage approval" inside a sentence-long string, where a
/// `<repo>/target` path is unlikely. All of these must hold for the token around `i`:
/// - it is letter-only words joined by `/`, `target` is not the last word, and the word after
///   `target` is not a cargo output folder (so `target/release` and `x/target` still fail);
/// - whitespace sits on both sides of it (so `"harness/target"` and `Path("a/target/b")`, a
///   whole quoted path or call argument, still fail);
/// - it lies inside a quoted string of at least [`PROSE_WORDS`] words (so an unquoted shell
///   argument such as `cp target/foo out` still fails).
fn prose_slash_list(line: &str, i: usize) -> bool {
    let start = token_start(line, i);
    let tok = token(line, i);
    let end = start + tok.len();
    let words: Vec<&str> = tok.split('/').collect();
    let plain = words.len() > 1
        && words
            .iter()
            .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphabetic()))
        && words.last() != Some(&"target")
        && words
            .windows(2)
            .all(|pair| pair[0] != "target" || !CARGO_OUTPUT_DIRS.contains(&pair[1]));
    let spaced = prev_char(line, start).is_some_and(char::is_whitespace)
        && next_char(line, end).is_none_or(|c| c.is_whitespace() || ",;".contains(c));
    if !(plain && spaced) {
        return false;
    }
    let Some(open) = line[..start].rfind(['"', '\'']) else {
        return false;
    };
    let quote = &line[open..open + 1];
    line[end..]
        .find(quote)
        .is_some_and(|close| line[open + 1..end + close].split_whitespace().count() >= PROSE_WORDS)
}

fn repo_target(line: &str) -> bool {
    line.match_indices("target").any(|(i, _)| {
        if prose_slash_list(line, i) {
            return false;
        }
        let prev = prev_char(line, i);
        let next = next_char(line, i + "target".len());
        let next_sep = matches!(next, Some('/' | '\\'));
        let next_end = next.is_none_or(|c| ends_token(c) || "`:}".contains(c));
        if token(line, i).contains("DEV_HOST_ROOT") {
            return false;
        }
        match prev {
            Some('/' | '\\') => next_sep || next_end,
            Some(q @ ('"' | '\'')) if next == Some(q) => {
                let before = line[..i - 1].trim_end();
                let after = line[i + "target".len() + 1..].trim_start();
                before.ends_with('/')
                    || before.ends_with("Path(")
                    || after.starts_with('/')
                    || (before.ends_with(',') && before.contains("join("))
            }
            p if starts_path(p) => next_sep,
            _ => false,
        }
    })
}

fn machine_path(line: &str, rules: &Rules) -> bool {
    let hit = |i: usize| !rules.allowed(from_token(line, i));
    let rooted = |needle: &str, needs_sep_or_end: bool| {
        line.match_indices(needle).any(|(i, _)| {
            let next = next_char(line, i + needle.len());
            starts_path(prev_char(line, i))
                && (!needs_sep_or_end
                    || next.is_none_or(|c| c == '/' || c == '\\' || ends_token(c) || c == ':'))
                && hit(i)
        })
    };
    if rooted("/Users/", false)
        || rooted("/Volumes/", false)
        || rooted("/private/tmp", true)
        || rooted("/tmp", true)
        || rooted("~/", false)
    {
        return true;
    }
    let home_slash = line
        .match_indices("$HOME")
        .any(|(i, _)| matches!(next_char(line, i + "$HOME".len()), Some('/' | '\\')) && hit(i));
    if home_slash || line.contains("${HOME}") {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    if [
        "%userprofile%",
        "%public%",
        "$env:userprofile",
        "$env:public",
        "path.home()",
        "homedir()",
        "expanduser(",
    ]
    .iter()
    .any(|n| lower.contains(n))
    {
        return true;
    }
    // Drive paths: `C:\…`, `E:/…` (not `x://`), starting a path.
    let bytes = line.as_bytes();
    (0..bytes.len().saturating_sub(2)).any(|i| {
        bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && (bytes[i + 2] == b'\\' || (bytes[i + 2] == b'/' && bytes.get(i + 3) != Some(&b'/')))
            && line.is_char_boundary(i)
            && starts_path(prev_char(line, i))
            && prev_char(line, i) != Some(':')
            && hit(i)
    })
}

/// Matches `/`-separated paths against a glob with `**` (any number of folders), `*` and `?`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    match_segments(&p, &s)
}

fn match_segments(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|k| match_segments(rest, &s[k..])),
        Some((first, rest)) => {
            !s.is_empty()
                && match_segment(first.as_bytes(), s[0].as_bytes())
                && match_segments(rest, &s[1..])
        }
    }
}

fn match_segment(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|k| match_segment(rest, &s[k..])),
        Some((b'?', rest)) => !s.is_empty() && match_segment(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && match_segment(rest, &s[1..]),
    }
}

fn is_doc(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".md", ".mdx", ".markdown"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Whether the path rules apply to `path` (repo-relative, `/`-separated).
pub fn in_scope(path: &str, globs: &[String]) -> bool {
    !is_doc(path) && globs.iter().any(|g| glob_match(g, path))
}

/// `(file, line number, text)` for each added line of a `git diff -U0` output.
pub fn added_lines(diff: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let mut file: Option<String> = None;
    let mut in_header = false;
    let mut next = 0usize;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
            file = None;
        } else if in_header && let Some(name) = line.strip_prefix("+++ ") {
            let name = name.trim_matches('"');
            file = name.strip_prefix("b/").map(str::to_owned);
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            in_header = false;
            next = hunk
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if !in_header && let Some(text) = line.strip_prefix('+') {
            if let Some(f) = &file {
                out.push((f.clone(), next, text.to_owned()));
            }
            next += 1;
        } else if !in_header && line.starts_with(' ') {
            next += 1;
        }
    }
    out
}

fn git(top: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// The repository's top folder.
pub fn repo_top(dir: &Path) -> Result<PathBuf, String> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
}

/// Checks the lines added since the merge base of `base` and `HEAD`, working tree included.
pub fn check_diff(
    top: &Path,
    base: &str,
    globs: &[String],
    rules: &Rules,
) -> Result<Vec<Finding>, String> {
    let diff = git(
        top,
        &[
            "diff",
            "--merge-base",
            base,
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ],
    )?;
    let diff = String::from_utf8_lossy(&diff);
    Ok(added_lines(&diff)
        .into_iter()
        .filter(|(file, _, _)| in_scope(file, globs))
        .filter_map(|(file, line, text)| {
            check_line(&text, rules).map(|reason| Finding {
                file,
                line,
                reason,
                text,
            })
        })
        .collect())
}

/// Checks every line of every tracked file in scope.
pub fn check_all(top: &Path, globs: &[String], rules: &Rules) -> Result<Vec<Finding>, String> {
    let listing = git(top, &["ls-files", "-z"])?;
    let mut out = Vec::new();
    for name in listing.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name).into_owned();
        if !in_scope(&name, globs) {
            continue;
        }
        let Ok(bytes) = std::fs::read(top.join(&name)) else {
            continue;
        };
        if bytes.iter().take(8000).any(|b| *b == 0) {
            continue;
        }
        for (n, text) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if let Some(reason) = check_line(text, rules) {
                out.push(Finding {
                    file: name.clone(),
                    line: n + 1,
                    reason,
                    text: text.to_owned(),
                });
            }
        }
    }
    Ok(out)
}

/// An E12 item: its description and a test over the marked text.
pub type DocItem = (&'static str, fn(&str) -> bool);

/// The E12 items.
pub fn doc_items() -> Vec<DocItem> {
    fn word(text: &str, w: &str) -> bool {
        text.match_indices(w)
            .any(|(i, _)| prev_char(text, i).is_none_or(|c| !c.is_alphanumeric()))
    }
    vec![
        ("the root variable `DEV_HOST_ROOT`", |t| {
            t.contains("DEV_HOST_ROOT")
        }),
        (
            "the layout (`build/`, `t/`, `cache/`, `weights/`, `campaigns/`, `locks/`)",
            |t| {
                ["build/", "t/", "cache/", "weights/", "campaigns/", "locks/"]
                    .iter()
                    .all(|w| word(t, w))
            },
        ),
        ("the `dev-host` tool (`dev-host env`)", |t| {
            t.contains("dev-host env")
        }),
        ("the allow-path comment (`dev-host: allow-path`)", |t| {
            t.contains(ALLOW_MARK)
        }),
        (
            "the rule never to search from `/` or `$HOME` (\"never search\" and `$HOME`)",
            |t| {
                let l = t.to_ascii_lowercase();
                l.contains("never search") && l.contains("$home")
            },
        ),
    ]
}

fn marked_section(text: &str) -> Option<&str> {
    let start = text.find(DOC_START)? + DOC_START.len();
    let rest = &text[start..];
    Some(rest.find(DOC_END).map_or(rest, |end| &rest[..end]))
}

/// What the repo's agent docs lack; empty when every E12 item is present.
pub fn check_docs(top: &Path) -> Vec<String> {
    let agents = std::fs::read_to_string(top.join("AGENTS.md"));
    let Ok(agents) = agents else {
        return vec!["AGENTS.md is missing".to_owned()];
    };
    let Some(section) = marked_section(&agents) else {
        return vec![format!(
            "AGENTS.md has no `{DOC_START}` … `{DOC_END}` section"
        )];
    };
    let mut text = section.to_owned();
    if let Ok(fd) = std::fs::read_to_string(top.join("FEATURE_DEVELOPMENT.md"))
        && let Some(more) = marked_section(&fd)
    {
        text.push('\n');
        text.push_str(more);
    }
    doc_items()
        .into_iter()
        .filter(|(_, present)| !present(&text))
        .map(|(item, _)| format!("the dev-host section lacks {item}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    fn reason(line: &str) -> Option<&'static str> {
        check_line(line, &Rules::default())
    }

    #[track_caller]
    fn fails(line: &str, expected: &'static str) {
        assert_eq!(reason(line), Some(expected), "line: {line}");
    }

    #[track_caller]
    fn passes(line: &str) {
        assert_eq!(reason(line), None, "line: {line}");
    }

    #[test]
    fn machine_paths_fail() {
        for line in [
            "cp out /Users/runner/models/x",
            "export HF_HOME=$HOME/foo",
            r#"path="${HOME}/cache""#,
            "ls ~/models",
            "TMP=/tmp/build",
            "mktemp -d /tmp",
            "rm -rf /private/tmp/sceneworks",
            "MODELS=/Volumes/Models/huggingface",
            r"set LOCK=%PUBLIC%\SceneWorks\locks",
            r"copy x %USERPROFILE%\cache",
            r"$dir = Join-Path $env:USERPROFILE 'x'",
            r"New-Item C:\runner\cache",
            r"Copy-Item E:\foo .",
            "cache: E:/models",
            "root = Path.home() / 'models'",
            "base = os.path.expanduser('~/x')",
            "const dir = os.homedir();",
        ] {
            fails(line, R_MACHINE_PATH);
        }
    }

    #[test]
    fn machine_path_lookalikes_pass() {
        for line in [
            "ROOT=$DEV_HOST_ROOT/t/abc",
            r"set ROOT=%DEV_HOST_ROOT%\locks",
            "url: https://example.com/Users/x",
            "ls ./foo/tmp/x",
            "echo $HOMEBREW_PREFIX/bin",
            "uses: actions/checkout@v4",
            "run: /usr/bin/env python3 x.py",
            "PATH=/opt/homebrew/bin:$PATH",
            "/bin/sh -c true",
            r"& 'C:\Program Files\Git\bin\bash.exe' -lc true",
            "name: http://example.com",
            r#"const smi = path.join(env.SystemRoot ?? "C:\\Windows", "System32");"#,
            "exe: 'C:/Windows/powershell.exe'",
        ] {
            passes(line);
        }
    }

    #[test]
    fn app_data_fails() {
        for line in [
            "DATA=~/SceneWorks/data",
            "ls $HOME/SceneWorks",
            r#"x="${HOME}/SceneWorks/x""#,
            "rm /Users/someone/SceneWorks/data/x",
            "d = ~/Library/Application Support/SceneWorks/data",
            r#"p = Path.home() / "SceneWorks" / "data""#,
        ] {
            fails(line, R_APP_DATA);
        }
        passes("SCENEWORKS_DATA_DIR=$DEV_HOST_ROOT/t/x/SceneWorks/data");
        // Machine paths, but not the user's app data.
        fails("ROOT=/Users/Shared/SceneWorks/x", R_MACHINE_PATH);
        fails("t=$HOME/sceneworks-campaign-target/x", R_MACHINE_PATH);
    }

    #[test]
    fn absolute_cargo_fails() {
        for line in [
            "~/.cargo/bin/cargo build",
            r#"cmd = [str(Path.home()/".cargo/bin/cargo"), "test"]"#,
            r#"cargo = Path.home() / ".cargo" / "bin" / "cargo""#,
            r"& $env:CARGO_HOME\bin\cargo.exe build",
            "$CARGO_HOME/bin/cargo test",
        ] {
            fails(line, R_ABS_CARGO);
        }
        for line in [
            "cargo build --release",
            "uses: actions-rs/cargo@v1",
            "cat .cargo/config.toml",
            "/usr/local/bin/cargo build",
            "ls ~/.cargo-target",
            "assert.match(job, /cargo build --release/);",
        ] {
            assert_ne!(reason(line), Some(R_ABS_CARGO), "line: {line}");
        }
    }

    #[test]
    fn multibyte_text_before_a_match_is_handled() {
        fails("\u{feff}x é/cargo", R_ABS_CARGO);
        fails("\u{feff}/tmp/x", R_MACHINE_PATH);
    }

    #[test]
    fn target_dir_outside_root_fails() {
        for line in [
            "export CARGO_TARGET_DIR=../cargo-target",
            "  CARGO_TARGET_DIR: ${{ github.workspace }}/../cargo-target",
            r#"env["CARGO_TARGET_DIR"] = str(tmp / "t")"#,
            r#"env = {"CARGO_TARGET_DIR": build_dir}"#,
            r#"cmd.env("CARGO_TARGET_DIR", &dir);"#,
            r#"os.environ.setdefault("CARGO_TARGET_DIR", "out")"#,
            "$env:CARGO_TARGET_DIR = 'D:\\t'",
            "cargo build --target-dir out/x",
        ] {
            fails(line, R_TARGET_DIR);
        }
        for line in [
            "export CARGO_TARGET_DIR=$DEV_HOST_ROOT/build/x",
            "  CARGO_TARGET_DIR: ${{ env.DEV_HOST_ROOT }}/build/x",
            r#"CARGO_TARGET_DIR="$(dev-host env --print target)""#,
            "ls $CARGO_TARGET_DIR/release",
            "echo ${CARGO_TARGET_DIR}",
            "path: ${{ env.CARGO_TARGET_DIR }}/release/app",
            r#"td = os.environ.get("CARGO_TARGET_DIR")"#,
            r#"if env.get("CARGO_TARGET_DIR") == x:"#,
            "unset CARGO_TARGET_DIR",
            "cargo build --target-dir $DEV_HOST_ROOT/build/y",
        ] {
            passes(line);
        }
    }

    #[test]
    fn repo_target_assumptions_fail() {
        for line in [
            "ls ./target/release/app",
            "cp target/debug/x out",
            r"copy target\release\x.exe out",
            "BIN=$GITHUB_WORKSPACE/target/release/sw",
            "path: ${{ github.workspace }}/target",
            r#"out = repo_root / "target" / "release""#,
            r#"const out = path.join(root, "target", "debug");"#,
            r#"p = Path("target") / "x""#,
            "ls target/release",
            "cp target/debug/app out/app",
            "du -sh target/doc",
            "exe=root/'target/release'/f'eval-{engine}'",
            r#"build_dir = Path("harness/target")"#,
            r#"out = "foo/target""#,
            r#"p = Path("a/target/b")"#,
            "rm -rf out/target",
            "cp target/foo out",
            r#"echo "a b c d out/target e""#,
            r#""Compare selected source/target records in a sentence""#,
            r#"x = "one two three target/release four five""#,
        ] {
            fails(line, R_REPO_TARGET);
        }
        for line in [
            "cargo build --target aarch64-apple-darwin",
            "rustup target add x86_64-pc-windows-msvc",
            r#"{"target": "x86_64"}"#,
            "path: ${{ env.CARGO_TARGET_DIR }}/release/app",
            "echo ${{ matrix.target }}",
            "retarget/stuff",
            "ls targets/",
            r#""rules": "Exact target/digest approval where required and more.""#,
            r#"'Replacement names target/revision and creates immutable retained version'"#,
            r#""Schema revision 2: the record also carries operation/target/stage, too""#,
        ] {
            assert_ne!(reason(line), Some(R_REPO_TARGET), "line: {line}");
        }
    }

    #[test]
    fn allow_path_comments_and_docs_are_exempt() {
        passes(
            "LOCK=/Volumes/Models/dev-host/locks/apple-metal.lock # dev-host: allow-path E3 fixed lock",
        );
        passes("assert p == '/tmp/x'  // dev-host: allow-path test asserts an existing path");
        fails("ls /tmp/x # dev-host: allow-path", R_ALLOW_REASON);
        passes("# cache lives in ~/models today");
        passes("  // see /Users/x/notes");
        passes(" * ./target/release is the old layout");
        assert!(!in_scope("scripts/README.md", &defaults()));
        assert!(in_scope("scripts/run.sh", &defaults()));
        assert!(in_scope(".github/workflows/ci.yml", &defaults()));
        assert!(in_scope("apps/desktop/scripts/build.mjs", &defaults()));
        assert!(!in_scope("crates/x/src/lib.rs", &defaults()));
    }

    #[test]
    fn allow_prefix_exempts_a_fixed_path() {
        let rules = Rules::new(&["/Volumes/Models/dev-host/locks".to_owned()]);
        assert_eq!(
            check_line(
                "LOCK=/Volumes/Models/dev-host/locks/apple-metal.lock",
                &rules
            ),
            None
        );
        assert_eq!(
            check_line("X=/Volumes/Models/dev-host/lockstep", &rules),
            Some(R_MACHINE_PATH)
        );
    }

    fn defaults() -> Vec<String> {
        DEFAULT_PATHS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn globs_match_folders() {
        assert!(glob_match(".github/**", ".github/workflows/a.yml"));
        assert!(glob_match("**/scripts/**", "scripts/a.sh"));
        assert!(glob_match("**/scripts/**", "apps/x/scripts/a/b.py"));
        assert!(glob_match("tools/*.py", "tools/a.py"));
        assert!(!glob_match("tools/*.py", "tools/x/a.py"));
        assert!(glob_match("**", "anything/at/all"));
    }

    #[test]
    fn parses_added_lines_with_numbers() {
        let diff = "\
diff --git a/scripts/a.sh b/scripts/a.sh
index 1..2 100644
--- a/scripts/a.sh
+++ b/scripts/a.sh
@@ -3,0 +4,2 @@ ctx
+ls /tmp/x
++++ not a header
@@ -10 +12 @@
-old
+new
diff --git a/gone.sh b/gone.sh
--- a/gone.sh
+++ /dev/null
@@ -1 +0,0 @@
-x
";
        let lines = added_lines(diff);
        assert_eq!(
            lines,
            vec![
                ("scripts/a.sh".into(), 4, "ls /tmp/x".into()),
                ("scripts/a.sh".into(), 5, "+++ not a header".into()),
                ("scripts/a.sh".into(), 12, "new".into()),
            ]
        );
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn diff_mode_reports_only_added_lines_in_scope() {
        let scratch = Scratch::new("guard-diff");
        let repo = scratch.path();
        git_in(repo, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(repo.join("scripts/a.sh"), "ls /tmp/old\n").unwrap();
        git_in(repo, &["add", "."]);
        git_in(repo, &["commit", "-q", "-m", "base"]);
        git_in(repo, &["checkout", "-q", "-b", "pr"]);
        std::fs::write(
            repo.join("scripts/a.sh"),
            "ls /tmp/old\necho ok\nls $HOME/foo\n",
        )
        .unwrap();
        std::fs::write(repo.join("scripts/NOTES.md"), "ls ~/x\n").unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/x.sh"), "ls ~/x\n").unwrap();
        git_in(repo, &["add", "."]);
        git_in(repo, &["commit", "-q", "-m", "pr"]);
        let top = repo_top(repo).unwrap();
        let found = check_diff(&top, "main", &defaults(), &Rules::default()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].file.as_str(), found[0].line), ("scripts/a.sh", 3));
        let all = check_all(&top, &defaults(), &Rules::default()).unwrap();
        assert_eq!(all.len(), 2, "{all:?}");
        let everything = check_all(&top, &["**".to_owned()], &Rules::default()).unwrap();
        assert_eq!(everything.len(), 3, "{everything:?}");
    }

    #[test]
    fn docs_need_every_item_in_the_marked_section() {
        let scratch = Scratch::new("guard-docs");
        let top = scratch.path();
        assert_eq!(check_docs(top), vec!["AGENTS.md is missing".to_owned()]);
        std::fs::write(top.join("AGENTS.md"), "# Notes\nDEV_HOST_ROOT\n").unwrap();
        assert!(check_docs(top)[0].contains("no `<!-- dev-host -->`"));
        let full = "<!-- dev-host -->\nState lives under `$DEV_HOST_ROOT`: `build/`, `t/`, \
                    `cache/`, `weights/`, `campaigns/`, `locks/`.\nRun `eval \"$(dev-host env)\"`.\n\
                    Mark exceptions with `dev-host: allow-path <reason>`.\n\
                    Never search from `/` or `$HOME`.\n<!-- /dev-host -->\n";
        std::fs::write(top.join("AGENTS.md"), full).unwrap();
        assert!(check_docs(top).is_empty(), "{:?}", check_docs(top));
        // Items may be split across AGENTS.md and FEATURE_DEVELOPMENT.md.
        let (head, tail) = full.split_once("Never").unwrap();
        std::fs::write(top.join("AGENTS.md"), format!("{head}{DOC_END}\n")).unwrap();
        let missing = check_docs(top);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(missing[0].contains("never to search"));
        std::fs::write(
            top.join("FEATURE_DEVELOPMENT.md"),
            format!("{DOC_START}\nNever{tail}"),
        )
        .unwrap();
        assert!(check_docs(top).is_empty());
        // Text outside the marked section does not count.
        std::fs::write(
            top.join("AGENTS.md"),
            format!("dev-host env\n{DOC_START}\nDEV_HOST_ROOT\n{DOC_END}\n"),
        )
        .unwrap();
        assert!(check_docs(top).len() >= 3);
    }
}
