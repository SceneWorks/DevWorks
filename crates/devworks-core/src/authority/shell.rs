//! Shell/process and declared-effect preflight input (SEC-05).
//!
//! Turns an argv (including `sh -c`, `cmd /c` and PowerShell `-Command` scripts) or a declared
//! MCP/package effect into [`EffectPlan`]s for [`super::destructive::preflight`], or into a
//! refusal. It models what it claims to model and fails closed on the rest:
//!
//! * **Modeled:** command lists and pipelines; `!`, `if`/`while`/`until`/`for` bodies, `( … )`
//!   and `{ …; }` groups (every branch is checked); `$( … )` and backtick bodies (analyzed as
//!   scripts); option arguments of `env`, `sudo`, `doas`, `timeout`, `nice`, `nohup`, `command`,
//!   `exec`, `time` and `xargs`; `rm`/`rmdir`/`unlink`/`shred`/`dd of=`, `del`/`erase`/`rd`,
//!   `Remove-Item` and its aliases; `find` global options and `-delete`/`-exec` expressions;
//!   `mv`/`Move-Item`; `rsync --delete`/`--remove-source-files`; `git clean`,
//!   `git worktree remove` and forced or deleting `git push`; `>` redirections.
//! * **Held (`NeedsInput`):** any working-directory change (`cd`, `pushd`, `popd`, `chdir`,
//!   `Set-Location`, `sl`, `env -C`, `sudo -D`) followed by a relative destructive target;
//!   runtime expansion (`$VAR`, `%VAR%`, substitutions) or brace expansion in a destructive
//!   operand; deletion fed by a pipeline (`xargs rm`, `… | Remove-Item`) or a piped shell;
//!   `case`/`select`/`function`/`coproc`; `find -L`/`-follow`; encoded PowerShell; wrappers it
//!   does not model (`stdbuf`, `chroot`, `Invoke-Expression`, …) around a deleting program;
//!   forced pushes whose destination cannot be resolved.
//! * **Refused:** forced, `+ref` or deleting pushes of a protected branch (`main`, `master`, or
//!   a remote default branch the caller knows), compared case-insensitively.
//!
//! Recognition is deliberately incomplete: arbitrary scripts, interpreters and native programs
//! cannot be proven safe, so an unrecognized command is reported with its residual risk
//! ([`record_assessment`] records it), never as harmless.
use super::destructive::{DestructiveKind, EffectPlan, InputNeeded, IntendedScope};
use super::policy::PROTECTED_REFS;
use super::redact::Redactor;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{NewEvent, StorageError, WriteTransaction};
use crate::protocol::{EventId, Id};
use serde_json::json;

/// Residual risk always attached to a command the parser did not fully explain.
pub const RESIDUAL_RISK: &str = "Arbitrary programs and scripts are not proven harmless by command inspection; with Full access they run with your user rights.";

#[derive(Debug)]
pub enum ShellAssessment {
    /// Recognized destructive steps to preflight, plus the residual risk of the rest.
    Destructive {
        plans: Vec<EffectPlan>,
        residual: &'static str,
    },
    /// No recognized destructive step. Not proof of harmlessness.
    NoRecognizedDestruction { residual: &'static str },
    /// A destructive step whose targets cannot be determined before dispatch.
    NeedsInput(InputNeeded),
    /// A forced or deleting push of a protected branch: refused like the typed Git command.
    ProtectedBranch { target: String },
}

/// Declared effect of an MCP or package tool call (classification input, not evidence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredEffect {
    pub destructive: bool,
    pub recursive: bool,
    /// Declared target paths; `None` when the tool did not declare them.
    pub targets: Option<Vec<String>>,
}

/// Opaque MCP/package tools with declared destructive intent and unresolved targets are held.
pub fn assess_declared(
    effect: &DeclaredEffect,
    working_directory: Option<&str>,
) -> ShellAssessment {
    if !effect.destructive {
        return ShellAssessment::NoRecognizedDestruction {
            residual: RESIDUAL_RISK,
        };
    }
    match &effect.targets {
        Some(t) if !t.is_empty() => ShellAssessment::Destructive {
            plans: vec![plan(
                if effect.recursive {
                    DestructiveKind::RemoveTree
                } else {
                    DestructiveKind::Unlink
                },
                t.clone(),
                working_directory,
            )],
            residual: RESIDUAL_RISK,
        },
        _ => ShellAssessment::NeedsInput(InputNeeded::UnresolvedDeclaredTarget),
    }
}

fn plan(kind: DestructiveKind, targets: Vec<String>, cwd: Option<&str>) -> EffectPlan {
    EffectPlan {
        kind,
        targets,
        working_directory: cwd.map(str::to_owned),
        scope: IntendedScope::Ordinary,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dialect {
    Posix,
    Cmd,
    PowerShell,
}

/// One shell word: its literal text and whether it depends on runtime or brace expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Word {
    text: String,
    dynamic: bool,
}
fn literal(s: &str) -> Word {
    Word {
        text: s.to_owned(),
        dynamic: false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(Word),
    /// `;`, `&`, `&&`, `||`, newline (`pipe` false) or `|` (`pipe` true).
    Separator {
        pipe: bool,
    },
    /// `(` or `)`.
    Group,
    Redirect {
        truncate: bool,
    },
}

struct Lexed {
    tokens: Vec<Token>,
    /// Bodies of `$( … )` and backtick substitutions, analyzed as scripts.
    substitutions: Vec<String>,
    /// An unterminated quote or substitution.
    unbalanced: bool,
}

fn tokenize(script: &str, dialect: Dialect) -> Lexed {
    let mut lx = Lexed {
        tokens: Vec::new(),
        substitutions: Vec::new(),
        unbalanced: false,
    };
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    let mut word = String::new();
    let mut unquoted = String::new();
    let mut in_word = false;
    let mut dynamic = false;
    let escape = match dialect {
        Dialect::Posix => '\\',
        Dialect::PowerShell => '`',
        Dialect::Cmd => '^',
    };
    let flush = |lx: &mut Lexed,
                 word: &mut String,
                 unquoted: &mut String,
                 in_word: &mut bool,
                 dynamic: &mut bool| {
        if *in_word {
            let brace = dialect != Dialect::Cmd && brace_expansion(unquoted);
            lx.tokens.push(Token::Word(Word {
                text: std::mem::take(word),
                dynamic: *dynamic || brace,
            }));
        }
        unquoted.clear();
        *in_word = false;
        *dynamic = false;
    };
    // Captures a `$( … )` body starting after the `(`; returns the index after `)`.
    let capture = |chars: &[char], mut j: usize, lx: &mut Lexed| -> usize {
        let mut depth = 1;
        let start = j;
        while j < chars.len() {
            match chars[j] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        lx.substitutions.push(chars[start..j].iter().collect());
                        return j + 1;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        lx.unbalanced = true;
        j
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            _ if c == escape => {
                in_word = true;
                if let Some(&n) = chars.get(i + 1) {
                    if !(dialect == Dialect::Posix && n == '\n') {
                        word.push(n);
                    }
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            '\'' if dialect != Dialect::Cmd => {
                in_word = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        closed = true;
                        break;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                lx.unbalanced |= !closed;
            }
            '"' => {
                in_word = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    let q = chars[i];
                    if q == '"' {
                        closed = true;
                        break;
                    }
                    if dialect == Dialect::Posix && q == '\\' && i + 1 < chars.len() {
                        word.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if q == '$' && dialect != Dialect::Cmd {
                        dynamic = true;
                        if chars.get(i + 1) == Some(&'(') {
                            i = capture(&chars, i + 2, &mut lx);
                            word.push_str("$(…)");
                            continue;
                        }
                    }
                    if q == '`' && dialect == Dialect::Posix {
                        dynamic = true;
                        let start = i + 1;
                        match chars[start..].iter().position(|&x| x == '`') {
                            Some(n) => {
                                lx.substitutions
                                    .push(chars[start..start + n].iter().collect());
                                i = start + n + 1;
                            }
                            None => {
                                lx.unbalanced = true;
                                i = chars.len();
                            }
                        }
                        continue;
                    }
                    if q == '%' && dialect == Dialect::Cmd {
                        dynamic = true;
                    }
                    word.push(q);
                    i += 1;
                }
                lx.unbalanced |= !closed;
            }
            '$' if dialect != Dialect::Cmd => {
                in_word = true;
                dynamic = true;
                if chars.get(i + 1) == Some(&'(') {
                    i = capture(&chars, i + 2, &mut lx);
                    word.push_str("$(…)");
                    continue;
                }
                word.push(c);
            }
            '`' if dialect == Dialect::Posix => {
                in_word = true;
                dynamic = true;
                let start = i + 1;
                match chars[start..].iter().position(|&x| x == '`') {
                    Some(n) => {
                        lx.substitutions
                            .push(chars[start..start + n].iter().collect());
                        i = start + n + 1;
                    }
                    None => {
                        lx.unbalanced = true;
                        i = chars.len();
                    }
                }
                continue;
            }
            '%' if dialect == Dialect::Cmd => {
                in_word = true;
                dynamic = true;
                word.push(c);
            }
            ';' | '\n' | '|' | '&' => {
                flush(
                    &mut lx,
                    &mut word,
                    &mut unquoted,
                    &mut in_word,
                    &mut dynamic,
                );
                if c == '&' && chars.get(i + 1) == Some(&'>') {
                    i += 2;
                    lx.tokens.push(Token::Redirect { truncate: true });
                    continue;
                }
                let pipe = c == '|' && chars.get(i + 1) != Some(&'|');
                while chars.get(i + 1) == Some(&c) {
                    i += 1;
                }
                lx.tokens.push(Token::Separator { pipe });
            }
            '(' | ')' => {
                flush(
                    &mut lx,
                    &mut word,
                    &mut unquoted,
                    &mut in_word,
                    &mut dynamic,
                );
                lx.tokens.push(Token::Group);
            }
            '>' => {
                // `2>` style descriptor prefixes are part of the redirection.
                if in_word && word.chars().all(|d| d.is_ascii_digit()) {
                    word.clear();
                    unquoted.clear();
                    in_word = false;
                    dynamic = false;
                } else {
                    flush(
                        &mut lx,
                        &mut word,
                        &mut unquoted,
                        &mut in_word,
                        &mut dynamic,
                    );
                }
                let append = chars.get(i + 1) == Some(&'>');
                if append {
                    i += 1;
                }
                lx.tokens.push(Token::Redirect { truncate: !append });
            }
            c if c.is_whitespace() => flush(
                &mut lx,
                &mut word,
                &mut unquoted,
                &mut in_word,
                &mut dynamic,
            ),
            _ => {
                in_word = true;
                word.push(c);
                unquoted.push(c);
            }
        }
        i += 1;
    }
    flush(
        &mut lx,
        &mut word,
        &mut unquoted,
        &mut in_word,
        &mut dynamic,
    );
    lx
}

/// Unquoted `{a,b}` or `{1..3}` (brace expansion), not a lone `{`/`}` word.
fn brace_expansion(unquoted: &str) -> bool {
    let Some(open) = unquoted.find('{') else {
        return false;
    };
    let Some(close) = unquoted[open..].find('}') else {
        return false;
    };
    let inner = &unquoted[open + 1..open + close];
    inner.contains(',') || inner.contains("..")
}

fn program(word: &str) -> String {
    let base = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_owned()
}

/// Programs whose invocation can delete, move away or rewrite targets or run other programs.
fn consequential(name: &str) -> bool {
    deleter(name)
        || matches!(
            name,
            "find"
                | "git"
                | "mv"
                | "move"
                | "move-item"
                | "mi"
                | "rsync"
                | "dd"
                | "sh"
                | "bash"
                | "zsh"
                | "dash"
                | "ksh"
                | "fish"
                | "ash"
                | "cmd"
                | "powershell"
                | "pwsh"
                | "xargs"
                | "env"
                | "sudo"
                | "doas"
        )
}
fn deleter(name: &str) -> bool {
    matches!(
        name,
        "rm" | "rmdir" | "unlink" | "shred" | "del" | "erase" | "rd" | "remove-item" | "ri"
    )
}
fn cwd_changer(name: &str) -> bool {
    matches!(
        name,
        "cd" | "chdir"
            | "pushd"
            | "popd"
            | "set-location"
            | "sl"
            | "push-location"
            | "pop-location"
    )
}
/// Wrappers that run another program but whose options this module does not model.
fn unmodeled_wrapper(name: &str) -> bool {
    matches!(
        name,
        "stdbuf"
            | "ionice"
            | "chrt"
            | "taskset"
            | "setsid"
            | "flock"
            | "watch"
            | "strace"
            | "ltrace"
            | "unbuffer"
            | "script"
            | "su"
            | "runuser"
            | "chroot"
            | "nsenter"
            | "unshare"
            | "firejail"
            | "bwrap"
            | "systemd-run"
            | "parallel"
            | "caffeinate"
            | "nocorrect"
            | "noglob"
            | "busybox"
            | "start"
            | "start-process"
            | "saps"
            | "invoke-command"
            | "icm"
            | "invoke-expression"
            | "iex"
            | "eval"
            | "source"
            | "."
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cwd {
    Known(Option<String>),
    /// A working-directory change happened; relative targets cannot be resolved.
    Unknown,
}

struct Collector<'p> {
    plans: Vec<EffectPlan>,
    needs: Option<InputNeeded>,
    protected: Option<String>,
    cwd: Cwd,
    protected_branches: &'p [String],
}
impl Collector<'_> {
    fn hold(&mut self, n: InputNeeded) {
        self.needs.get_or_insert(n);
    }
    fn cwd(&self) -> Option<&str> {
        match &self.cwd {
            Cwd::Known(c) => c.as_deref(),
            Cwd::Unknown => None,
        }
    }
    /// Literal operands of a destructive step, or a hold when they cannot be resolved.
    fn operands(&mut self, words: &[Word]) -> Option<Vec<String>> {
        if words.iter().any(|w| w.dynamic) {
            self.hold(InputNeeded::UnresolvedExpansion);
            return None;
        }
        if self.cwd == Cwd::Unknown && words.iter().any(|w| !absolute(&w.text)) {
            self.hold(InputNeeded::UnresolvedWorkingDirectory);
            return None;
        }
        Some(words.iter().map(|w| w.text.clone()).collect())
    }
    fn push(&mut self, kind: DestructiveKind, words: &[Word]) {
        if let Some(t) = self.operands(words) {
            let cwd = self.cwd().map(str::to_owned);
            self.plans.push(plan(kind, t, cwd.as_deref()));
        }
    }
}

fn absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with('\\')
        || p == "~"
        || p.starts_with("~/")
        || p.starts_with("~\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

/// Assesses one argv as dispatched (no implicit shell). `main`/`master` are protected.
pub fn assess_command(argv: &[String], working_directory: Option<&str>) -> ShellAssessment {
    assess_command_with(argv, working_directory, &[])
}

/// [`assess_command`] with additional protected branch names (for example the remote default
/// branch when the Git adapter knows it).
pub fn assess_command_with(
    argv: &[String],
    working_directory: Option<&str>,
    protected_branches: &[String],
) -> ShellAssessment {
    let words: Vec<Word> = argv.iter().map(|a| literal(a)).collect();
    let mut c = Collector {
        plans: Vec::new(),
        needs: None,
        protected: None,
        cwd: Cwd::Known(working_directory.map(str::to_owned)),
        protected_branches,
    };
    command(&words, Dialect::Posix, false, &mut c, 0);
    if let Some(target) = c.protected {
        return ShellAssessment::ProtectedBranch { target };
    }
    if let Some(n) = c.needs {
        return ShellAssessment::NeedsInput(n);
    }
    if c.plans.is_empty() {
        ShellAssessment::NoRecognizedDestruction {
            residual: RESIDUAL_RISK,
        }
    } else {
        ShellAssessment::Destructive {
            plans: c.plans,
            residual: RESIDUAL_RISK,
        }
    }
}

const MAX_DEPTH: u8 = 8;

fn script(text: &str, dialect: Dialect, c: &mut Collector, depth: u8) {
    if depth > MAX_DEPTH {
        c.hold(InputNeeded::OpaqueCommand);
        return;
    }
    let lexed = tokenize(text, dialect);
    if lexed.unbalanced {
        c.hold(InputNeeded::OpaqueCommand);
    }
    for body in &lexed.substitutions {
        script(body, dialect, c, depth + 1);
    }
    let mut current: Vec<Word> = Vec::new();
    let mut redirect = None;
    // Whether the command being collected reads a pipeline, and whether the last separator was
    // a pipe.
    let mut piped = false;
    let mut after_pipe = false;
    let run = |current: &mut Vec<Word>, piped: bool, c: &mut Collector| {
        command(current, dialect, piped, c, depth + 1);
        current.clear();
    };
    for t in lexed.tokens {
        match t {
            Token::Separator { pipe } => {
                run(&mut current, piped, c);
                after_pipe = pipe;
            }
            // Group boundaries delimit commands; every grouped command is checked.
            Token::Group => run(&mut current, piped, c),
            Token::Redirect { truncate } => redirect = Some(truncate),
            Token::Word(w) => match redirect.take() {
                Some(truncate) => {
                    let sink = w.text.to_ascii_lowercase();
                    if sink == "/dev/null" || sink == "nul" || sink == "$null" {
                        continue;
                    }
                    if truncate {
                        c.push(DestructiveKind::Truncate, &[w]);
                    }
                }
                None => {
                    if current.is_empty() {
                        piped = after_pipe;
                        after_pipe = false;
                    }
                    current.push(w)
                }
            },
        }
    }
    run(&mut current, piped, c);
}

/// Shell keywords that only introduce or close a compound command; the commands they guard
/// are each checked.
fn transparent_keyword(w: &str) -> bool {
    matches!(
        w,
        "!" | "{"
            | "}"
            | "if"
            | "then"
            | "elif"
            | "else"
            | "fi"
            | "do"
            | "done"
            | "while"
            | "until"
            | "time"
    )
}

/// Skips option words of a wrapper; `with_arg` lists options that take a separate value.
/// Returns the index of the wrapped program, or `None` when an option means "no execution".
fn skip_options(words: &[Word], mut i: usize, with_arg: &[&str], stop: &[&str]) -> Option<usize> {
    while let Some(w) = words.get(i) {
        let t = w.text.as_str();
        if t == "--" {
            return Some(i + 1);
        }
        if !t.starts_with('-') || t == "-" {
            return Some(i);
        }
        if stop.contains(&t) {
            return None;
        }
        let long_value = t.starts_with("--") && t.contains('=');
        i += if with_arg.contains(&t) && !long_value {
            2
        } else {
            1
        };
    }
    Some(i)
}

fn command(words: &[Word], dialect: Dialect, piped: bool, c: &mut Collector, depth: u8) {
    if words.is_empty() {
        return;
    }
    if depth > MAX_DEPTH {
        c.hold(InputNeeded::OpaqueCommand);
        return;
    }
    let mut i = 0;
    loop {
        let Some(w) = words.get(i) else { return };
        let t = w.text.as_str();
        if dialect == Dialect::Posix && transparent_keyword(t) {
            i += 1;
            continue;
        }
        if dialect == Dialect::Posix && matches!(t, "case" | "select" | "function" | "coproc") {
            c.hold(InputNeeded::OpaqueCommand);
            return;
        }
        if dialect == Dialect::Posix && t == "for" {
            // A loop header executes nothing; its body is checked separately.
            return;
        }
        if dialect == Dialect::Posix
            && t.contains('=')
            && !t.starts_with('-')
            && !t.starts_with('=')
        {
            i += 1;
            continue;
        }
        let name = program(t);
        let next = match name.as_str() {
            "env" => {
                let mut j = i + 1;
                loop {
                    let Some(a) = words.get(j) else { return };
                    let a = a.text.as_str();
                    if a == "-C" || a == "--chdir" {
                        c.cwd = Cwd::Unknown;
                        j += 2;
                    } else if a.starts_with("--chdir=") || (a.starts_with("-C") && a.len() > 2) {
                        c.cwd = Cwd::Unknown;
                        j += 1;
                    } else if a == "-S" || a.starts_with("--split-string") || a.starts_with("-S") {
                        c.hold(InputNeeded::OpaqueCommand);
                        return;
                    } else if a == "-u" || a == "--unset" {
                        j += 2;
                    } else if a == "--" {
                        j += 1;
                        break;
                    } else if a.starts_with('-') || (a.contains('=') && !a.starts_with('=')) {
                        j += 1;
                    } else {
                        break;
                    }
                }
                Some(j)
            }
            "sudo" => {
                if words[i + 1..]
                    .iter()
                    .any(|a| a.text == "-D" || a.text.starts_with("--chdir"))
                {
                    c.cwd = Cwd::Unknown;
                }
                skip_options(
                    words,
                    i + 1,
                    &[
                        "-u",
                        "-g",
                        "-C",
                        "-D",
                        "-h",
                        "-p",
                        "-r",
                        "-t",
                        "-T",
                        "-U",
                        "--user",
                        "--group",
                        "--close-from",
                        "--chdir",
                        "--host",
                        "--prompt",
                        "--role",
                        "--type",
                        "--command-timeout",
                        "--other-user",
                    ],
                    &[
                        "-e",
                        "--edit",
                        "-l",
                        "--list",
                        "-v",
                        "--validate",
                        "-k",
                        "-K",
                        "-V",
                    ],
                )
            }
            "doas" => skip_options(words, i + 1, &["-u", "-C"], &["-C"]),
            "timeout" => skip_options(words, i + 1, &["-s", "-k", "--signal", "--kill-after"], &[])
                .map(|j| j + 1),
            "nice" => skip_options(words, i + 1, &["-n", "--adjustment"], &[]),
            "nohup" | "builtin" => skip_options(words, i + 1, &[], &[]),
            "command" => skip_options(words, i + 1, &[], &["-v", "-V"]),
            "exec" => skip_options(words, i + 1, &["-a"], &[]),
            "time" => skip_options(words, i + 1, &["-f", "-o", "--format", "--output"], &[]),
            "xargs" => {
                let j = skip_options(
                    words,
                    i + 1,
                    &[
                        "-a",
                        "-d",
                        "-E",
                        "-e",
                        "-I",
                        "-i",
                        "-L",
                        "-l",
                        "-n",
                        "-P",
                        "-s",
                        "--arg-file",
                        "--delimiter",
                        "--max-args",
                        "--max-procs",
                        "--max-chars",
                        "--max-lines",
                        "--replace",
                        "--eof",
                    ],
                    &[],
                );
                // Targets arrive on standard input.
                if let Some(j) = j
                    && words
                        .get(j)
                        .is_some_and(|w| consequential(&program(&w.text)))
                {
                    c.hold(InputNeeded::UnresolvedTarget);
                }
                return;
            }
            n if unmodeled_wrapper(n) => {
                if words[i + 1..]
                    .iter()
                    .any(|w| consequential(&program(&w.text)))
                    || matches!(n, "invoke-expression" | "iex" | "eval" | "source" | ".")
                {
                    c.hold(InputNeeded::OpaqueCommand);
                }
                return;
            }
            _ => None,
        };
        match next {
            Some(j) => i = j,
            None if matches!(
                name.as_str(),
                "env"
                    | "sudo"
                    | "doas"
                    | "timeout"
                    | "nice"
                    | "nohup"
                    | "builtin"
                    | "command"
                    | "exec"
                    | "time"
            ) =>
            {
                // An option meaning "do not execute" (`sudo -e`, `command -v`, …).
                return;
            }
            None => break,
        }
    }
    let head = &words[i];
    let name = program(&head.text);
    let args = &words[i + 1..];
    if head.dynamic {
        // The program itself is computed at run time.
        c.hold(InputNeeded::UnresolvedExpansion);
        return;
    }
    match name.as_str() {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" | "ash" => shell(args, piped, c, depth),
        "cmd" => {
            if let Some(k) = args.iter().position(|a| {
                let l = a.text.to_ascii_lowercase();
                l == "/c" || l == "/k" || l == "/r"
            }) {
                let body: Vec<&str> = args[k + 1..].iter().map(|a| a.text.as_str()).collect();
                script(&body.join(" "), Dialect::Cmd, c, depth);
            } else if piped {
                c.hold(InputNeeded::OpaqueCommand);
            }
        }
        "powershell" | "pwsh" => powershell(&name, args, piped, c, depth),
        n if cwd_changer(n) => c.cwd = Cwd::Unknown,
        "xargs" => {}
        "find" => find(args, c),
        "git" => git(args, c),
        "mv" | "move" | "move-item" | "mi" => move_away(args, dialect, c),
        "rsync" => rsync(args, c),
        "dd" => {
            let outputs: Vec<Word> = args
                .iter()
                .filter_map(|a| {
                    a.text.strip_prefix("of=").map(|t| Word {
                        text: t.to_owned(),
                        dynamic: a.dynamic,
                    })
                })
                .collect();
            if !outputs.is_empty() {
                c.push(DestructiveKind::Truncate, &outputs);
            }
        }
        n if deleter(n)
            || (dialect == Dialect::PowerShell && matches!(n, "rm" | "rmdir" | "del")) =>
        {
            removal(&name, args, dialect, piped, c)
        }
        _ => {}
    }
}

fn shell(args: &[Word], piped: bool, c: &mut Collector, depth: u8) {
    let mut j = 0;
    while let Some(a) = args.get(j) {
        let t = a.text.as_str();
        if t == "-o" || t == "+o" || t == "--rcfile" || t == "--init-file" {
            j += 2;
            continue;
        }
        if t.starts_with('-') && !t.starts_with("--") && t.len() > 1 && t[1..].contains('c') {
            match args.get(j + 1) {
                Some(body) if body.dynamic => c.hold(InputNeeded::UnresolvedExpansion),
                Some(body) => script(&body.text, Dialect::Posix, c, depth + 1),
                None => c.hold(InputNeeded::OpaqueCommand),
            }
            return;
        }
        if t == "-s" || t == "-" {
            break;
        }
        if !t.starts_with('-') && !t.starts_with('+') {
            // A script file: opaque, reported as residual risk.
            return;
        }
        j += 1;
    }
    if piped {
        // A script read from a pipeline cannot be inspected before dispatch.
        c.hold(InputNeeded::OpaqueCommand);
    }
}

fn powershell(name: &str, args: &[Word], piped: bool, c: &mut Collector, depth: u8) {
    let mut j = 0;
    while let Some(a) = args.get(j) {
        let l = a.text.to_ascii_lowercase();
        let opt = l.trim_start_matches(['-', '/']);
        if !(l.starts_with('-') || l.starts_with('/')) {
            if name == "powershell" {
                let body: Vec<&str> = args[j..].iter().map(|a| a.text.as_str()).collect();
                script(&body.join(" "), Dialect::PowerShell, c, depth + 1);
            }
            return;
        }
        if opt == "ec" || (!opt.is_empty() && "encodedcommand".starts_with(opt)) {
            c.hold(InputNeeded::UnresolvedExpansion);
            return;
        }
        if !opt.is_empty() && "command".starts_with(opt) {
            match args.get(j + 1) {
                Some(n) if n.text == "-" => c.hold(InputNeeded::OpaqueCommand),
                Some(_) => {
                    let body: Vec<&str> = args[j + 1..].iter().map(|a| a.text.as_str()).collect();
                    script(&body.join(" "), Dialect::PowerShell, c, depth + 1);
                }
                None => c.hold(InputNeeded::OpaqueCommand),
            }
            return;
        }
        if !opt.is_empty() && "file".starts_with(opt) {
            return;
        }
        let takes_value = [
            "executionpolicy",
            "workingdirectory",
            "configurationname",
            "outputformat",
            "inputformat",
            "windowstyle",
            "settingsfile",
            "version",
            "psconsolefile",
        ]
        .iter()
        .any(|o| !opt.is_empty() && o.starts_with(opt) && opt.len() >= 2);
        if ["workingdirectory", "wd"]
            .iter()
            .any(|o| o.starts_with(opt) && opt.len() >= 2)
        {
            c.cwd = Cwd::Unknown;
        }
        j += if takes_value { 2 } else { 1 };
    }
    if piped {
        c.hold(InputNeeded::OpaqueCommand);
    }
}

fn find(args: &[Word], c: &mut Collector) {
    let mut j = 0;
    // Global options before the starting points.
    while let Some(a) = args.get(j) {
        match a.text.as_str() {
            "-L" | "-follow" => {
                // Deletion would follow links into their targets.
                c.hold(InputNeeded::UnresolvedTarget);
                return;
            }
            "-H" | "-P" => j += 1,
            "-D" => j += 2,
            t if t.starts_with("-O") => j += 1,
            _ => break,
        }
    }
    let starts: Vec<Word> = args[j..]
        .iter()
        .take_while(|a| {
            let t = a.text.as_str();
            !(t.starts_with('-') || t == "!" || t == "(" || t == ")" || t == ",")
        })
        .cloned()
        .collect();
    let expr = &args[j + starts.len()..];
    if expr.iter().any(|a| a.text == "-follow") {
        c.hold(InputNeeded::UnresolvedTarget);
        return;
    }
    let mut deletes = expr.iter().any(|a| a.text == "-delete");
    let mut k = 0;
    while k < expr.len() {
        if matches!(
            expr[k].text.as_str(),
            "-exec" | "-execdir" | "-ok" | "-okdir"
        ) {
            let inner: Vec<Word> = expr[k + 1..]
                .iter()
                .take_while(|a| a.text != ";" && a.text != "+")
                .cloned()
                .collect();
            if let Some(p) = inner.first() {
                let n = program(&p.text);
                if deleter(&n) {
                    deletes = true;
                } else if consequential(&n) {
                    c.hold(InputNeeded::OpaqueCommand);
                }
            }
            k += inner.len() + 1;
        }
        k += 1;
    }
    if deletes {
        let starts = if starts.is_empty() {
            vec![literal(".")]
        } else {
            starts
        };
        c.push(DestructiveKind::EraseContents, &starts);
    }
}

fn protected_branch(c: &Collector, dest: &str) -> bool {
    let name = dest.trim_start_matches('+');
    let name = name.strip_prefix("refs/heads/").unwrap_or(name);
    PROTECTED_REFS
        .iter()
        .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r))
        .chain(
            c.protected_branches
                .iter()
                .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r)),
        )
        .any(|p| p.eq_ignore_ascii_case(name))
}

fn git(args: &[Word], c: &mut Collector) {
    let mut j = 0;
    let mut dir: Option<Word> = None;
    while let Some(a) = args.get(j) {
        match a.text.as_str() {
            "-C" => {
                dir = args.get(j + 1).cloned();
                j += 2;
            }
            "-c" | "--git-dir" | "--work-tree" | "--namespace" => j += 2,
            t if t.starts_with("--git-dir=") || t.starts_with("--work-tree=") => j += 1,
            t if t.starts_with('-') => j += 1,
            _ => break,
        }
    }
    let Some(sub) = args.get(j) else { return };
    let rest = &args[j + 1..];
    match sub.text.as_str() {
        "clean" => {
            if rest.iter().any(|a| a.text == "-n" || a.text == "--dry-run") {
                return;
            }
            let paths: Vec<Word> = match rest.iter().position(|a| a.text == "--") {
                Some(p) => rest[p + 1..].to_vec(),
                None => rest
                    .iter()
                    .filter(|a| !a.text.starts_with('-'))
                    .cloned()
                    .collect(),
            };
            let base = dir.unwrap_or_else(|| literal("."));
            let targets = if paths.is_empty() {
                vec![base]
            } else if base.text == "." {
                paths
            } else {
                // Pathspecs relative to `-C`: hold rather than guess the join.
                c.hold(InputNeeded::UnresolvedTarget);
                return;
            };
            c.push(DestructiveKind::EraseContents, &targets);
        }
        "worktree" if rest.first().is_some_and(|a| a.text == "remove") => {
            let paths: Vec<Word> = rest[1..]
                .iter()
                .filter(|a| !a.text.starts_with('-'))
                .cloned()
                .collect();
            if dir.is_some() && paths.iter().any(|p| !absolute(&p.text)) {
                c.hold(InputNeeded::UnresolvedTarget);
                return;
            }
            c.push(DestructiveKind::RemoveTree, &paths);
        }
        "push" => {
            let mut force = false;
            let mut delete = false;
            let mut broad = false;
            let mut positional = Vec::new();
            let mut k = 0;
            while let Some(a) = rest.get(k) {
                k += 1;
                let t = a.text.as_str();
                match t {
                    "-f" | "--force" | "--force-if-includes" => force = true,
                    t if t.starts_with("--force-with-lease") => force = true,
                    "-d" | "--delete" => delete = true,
                    "--all" | "--mirror" | "--prune" | "--branches" => broad = true,
                    "-o" | "--push-option" | "--repo" | "--receive-pack" | "--exec" => k += 1,
                    t if t.starts_with('-') => {
                        if t.len() > 2 && !t.starts_with("--") && t[1..].contains('f') {
                            force = true;
                        }
                        if t.len() > 2 && !t.starts_with("--") && t[1..].contains('d') {
                            delete = true;
                        }
                    }
                    _ => positional.push(a.clone()),
                }
            }
            let refspecs = if positional.is_empty() {
                &[][..]
            } else {
                &positional[1..]
            };
            let mut rewrites = force || delete || broad;
            for r in refspecs {
                let t = r.text.as_str();
                let forced = t.starts_with('+');
                let dest = match t.trim_start_matches('+').split_once(':') {
                    Some((src, dest)) => {
                        if src.is_empty() {
                            rewrites = true;
                        }
                        dest
                    }
                    None => t.trim_start_matches('+'),
                };
                if (forced || force || delete || src_empty(t)) && protected_branch(c, dest) {
                    c.protected = Some(dest.to_owned());
                    return;
                }
                rewrites |= forced;
                if r.dynamic && (forced || force || delete) {
                    c.hold(InputNeeded::UnresolvedExpansion);
                }
            }
            if rewrites
                && (refspecs.is_empty()
                    || broad
                    || refspecs
                        .iter()
                        .any(|r| r.text.trim_start_matches('+') == "HEAD"))
            {
                // The rewritten destination cannot be determined before dispatch.
                c.hold(InputNeeded::UnresolvedTarget);
            }
        }
        _ => {}
    }
}
fn src_empty(refspec: &str) -> bool {
    refspec.trim_start_matches('+').starts_with(':')
}

fn move_away(args: &[Word], dialect: Dialect, c: &mut Collector) {
    let mut operands = Vec::new();
    let mut target_dir = false;
    let mut end = false;
    let mut j = 0;
    while j < args.len() {
        let a = &args[j];
        let t = a.text.as_str();
        let option = !end
            && ((t.starts_with('-') && t.len() > 1)
                || (dialect == Dialect::Cmd && t.starts_with('/') && t.len() <= 3));
        if !end && t == "--" {
            end = true;
        } else if !end && (t == "-t" || t == "--target-directory") {
            target_dir = true;
            j += 1;
        } else if !end && t.starts_with("--target-directory=") {
            target_dir = true;
        } else if option && dialect == Dialect::PowerShell && t.eq_ignore_ascii_case("-destination")
        {
            j += 1;
        } else if !option {
            operands.push(a.clone());
        }
        j += 1;
    }
    let sources = if target_dir || operands.len() < 2 {
        operands
    } else {
        operands[..operands.len() - 1].to_vec()
    };
    if !sources.is_empty() {
        c.push(DestructiveKind::Move, &sources);
    }
}

fn remote_path(p: &str) -> bool {
    p.starts_with("rsync://")
        || p.split_once(':')
            .is_some_and(|(host, _)| host.len() > 1 && !host.contains(['/', '\\']))
}

fn rsync(args: &[Word], c: &mut Collector) {
    let delete = args.iter().any(|a| {
        a.text.starts_with("--delete")
            || a.text.starts_with("--del")
            || a.text == "--remove-source-files"
    });
    if !delete {
        return;
    }
    let operands: Vec<Word> = args
        .iter()
        .filter(|a| !a.text.starts_with('-'))
        .cloned()
        .collect();
    let Some((dest, sources)) = operands.split_last() else {
        return;
    };
    if args.iter().any(|a| a.text.starts_with("--del")) && !remote_path(&dest.text) {
        c.push(DestructiveKind::EraseContents, std::slice::from_ref(dest));
    }
    if args.iter().any(|a| a.text == "--remove-source-files") {
        let local: Vec<Word> = sources
            .iter()
            .filter(|s| !remote_path(&s.text))
            .cloned()
            .collect();
        if !local.is_empty() {
            c.push(DestructiveKind::EraseContents, &local);
        }
    }
}

fn removal(name: &str, args: &[Word], dialect: Dialect, piped: bool, c: &mut Collector) {
    let cmd_style = dialect == Dialect::Cmd || matches!(name, "del" | "erase" | "rd");
    let powershell = dialect == Dialect::PowerShell || matches!(name, "remove-item" | "ri");
    let mut recursive = false;
    let mut operands = Vec::new();
    let mut end_of_flags = false;
    for a in args {
        let lower = a.text.to_lowercase();
        if !end_of_flags && a.text == "--" {
            end_of_flags = true;
        } else if !end_of_flags && cmd_style && lower.starts_with('/') && lower.len() <= 3 {
            recursive |= lower == "/s";
        } else if !end_of_flags && lower.starts_with("--") {
            recursive |= lower == "--recursive";
        } else if !end_of_flags && lower.starts_with('-') && lower.len() > 1 {
            // `-Path`/`-LiteralPath` values follow as ordinary operands.
            if powershell {
                recursive |= lower.starts_with("-r") && !lower.starts_with("-readonly");
            } else {
                recursive |= a.text[1..].contains(['r', 'R']);
            }
        } else {
            operands.push(a.clone());
        }
    }
    if operands.is_empty() {
        if powershell || piped {
            // Remove-Item takes its targets from the pipeline.
            c.hold(InputNeeded::UnresolvedTarget);
        }
        return;
    }
    let kind = match name {
        "unlink" => DestructiveKind::Unlink,
        "shred" => DestructiveKind::Truncate,
        "del" | "erase" if recursive && !powershell => DestructiveKind::EraseContents,
        _ if recursive => DestructiveKind::RemoveTree,
        _ => DestructiveKind::Unlink,
    };
    c.push(kind, &operands);
}

/// Records the assessment of a dispatched command in history, attributed to the invoking
/// principal: a refusal or hold with its reason, or (for an unrecognized command under Full
/// access) the residual risk it runs with. A recognized destructive step records nothing here;
/// its preflight decision does. The command text itself is not recorded; only the program name,
/// redacted, and a digest of the argv.
pub fn record_assessment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &str,
    argv: &[String],
    assessment: &ShellAssessment,
    profile: super::AccessProfile,
) -> Result<Option<i64>, StorageError> {
    let (kind, detail) = match assessment {
        ShellAssessment::Destructive { .. } => return Ok(None),
        ShellAssessment::ProtectedBranch { .. } => {
            ("DestructiveOperationBlocked", "ProtectedBranch")
        }
        ShellAssessment::NeedsInput(n) => ("DestructiveOperationHeld", n.as_str()),
        ShellAssessment::NoRecognizedDestruction { .. } => {
            if profile != super::AccessProfile::Full {
                return Ok(None);
            }
            ("NativeExecutionResidualRisk", "Unrecognized")
        }
    };
    let digest = sha256_hex(argv.join("\u{0}").as_bytes());
    let program = argv
        .first()
        .map(|p| Redactor::new().redact_text(&self::program(p)));
    let sources: Vec<&str> = ctx.authority_sources().iter().map(Id::as_str).collect();
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history()
        .append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: None,
            aggregate_kind: "shell_preflight".into(),
            aggregate_id: sha256_hex(
                format!("{}/{digest}", ctx.invocation_id().as_str()).as_bytes(),
            ),
            aggregate_revision: 1,
            kind: kind.into(),
            payload: json!({
                "operation": operation,
                "reason": detail,
                "program": program,
                "argvSha256": digest,
                "profile": profile.as_str(),
                "residualRisk": RESIDUAL_RISK,
                "principalKind": format!("{:?}", ctx.principal_kind()),
                "authoritySources": sources,
            }),
        })
        .map(Some)
}
