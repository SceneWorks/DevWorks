//! Shell/process and declared-effect preflight input (SEC-05).
//!
//! # Approach: structured analysis plus a fail-closed lexical backstop
//!
//! Modeling shell constructs one by one does not converge: every round of review found more
//! forms that came back as "nothing destructive". So the analysis has two layers.
//!
//! 1. **Structured analysis** turns the commands it understands completely into precise
//!    [`EffectPlan`]s (or a protected-branch refusal) for [`super::destructive::preflight`]:
//!    command lists, pipelines, `!`, `if`/`while`/`until`/`for` bodies, `( … )`/`{ …; }` groups,
//!    `$( … )`/backtick bodies; the option grammar of `env`, `sudo`, `doas`, `timeout`, `nice`,
//!    `nohup`, `command`, `exec`, `time`, `xargs` (combined short options included); `rm`,
//!    `rmdir`, `unlink`, `shred`, `dd of=`, `mv`, `rsync`, `find`, a set of `git` subcommands,
//!    cmd `rd`/`rmdir`/`del`/`erase` (joined switches such as `/s/q` included), PowerShell
//!    `Remove-Item`/`Move-Item` and their aliases, `[IO.Directory]::Delete('…')` with a literal
//!    path, and `>` redirections. A command counts as **fully modeled** only when every one of
//!    its words was understood: every option is known to that program's model, no word is
//!    dynamic (`$VAR`, substitutions, `~user`/`~+`/`~-`, brace expansion, a glob in the program
//!    word, `(` adjacent to a word, PowerShell commas/arrays/blocks), no semantics-changing
//!    environment assignment (`GIT_*`), and the working directory is known for relative targets.
//! 2. **Lexical backstop.** Independently, the raw input (argv words and every script text,
//!    quoted or not, including traps, aliases, heredocs, here-strings and process substitutions)
//!    is scanned for [consequential tokens](consequential_token): programs and verbs that delete,
//!    move or overwrite (`rm`, `mv`, `cp`, `ln`, `tee`, `truncate`, `rsync`, `find`, `git`, cmd
//!    `rd`/`del`/`move`/`ren`/`copy`, PowerShell `Remove-Item`/`Move-Item`/`Rename-Item`/
//!    `Set-Content`/`Clear-Content`/`Out-File`/`Copy-Item` and aliases, `[IO.*]::Delete/Move/…`).
//!    Every occurrence must be accounted for by a fully modeled command; otherwise the result is
//!    `NeedsInput(UnmodeledConsequentialCommand)`, never "nothing destructive".
//!
//! Interpreters with inline code (`perl -e`, `python -c`, `ruby -e`, `node -e`) are held when the
//! code names a deletion/move/write API (`unlink`, `rmtree`, `remove`, `rmdir`, `rm_rf`,
//! `Remove-Item`, `Delete`, …); shells fed by heredocs, here-strings or process substitution,
//! encoded PowerShell and `-CommandWithArgs` scripts are analyzed or held the same way.
//! Recognized consequential Git operations (`reset --hard`, `rm`, `update-ref -d`,
//! `branch -D`, `gc`, `prune`, …) are refused for protected branches and roots and held
//! otherwise; they are never allowed by default.
//!
//! **Precedence ([`preflight_command`]):** Blocked > NeedsInput > allowed plans > residual,
//! across every step and both layers. Resolvable operands are planned even when the command is
//! also held, so a refusable step (a filesystem root, a primary repository, Git metadata, a
//! protected branch) makes the whole command Blocked with every reason recorded
//! ([`record_verdict`]). Resolving a held command by approval must re-run the preflight and can
//! only dispatch [`CommandVerdict::dispatchable_after_approval`], which is empty for Blocked.
//!
//! **Trade-off, accepted:** benign scripts that merely mention these programs (`echo rm`, a
//! directory named `git`) are held more often. Residual risk ([`record_assessment`]) is reported
//! only for commands without any consequential token; arbitrary programs are still not proven
//! harmless by inspection.
use super::destructive::{
    DestructiveKind, EffectPlan, FilesystemResolver, InputNeeded, IntendedScope, PreflightDecision,
    PreflightVerdict, ProtectedRoots, ResolveError, ResolvedPath, preflight,
};
use super::policy::protected_ref;
use super::redact::Redactor;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{NewEvent, StorageError, WriteTransaction};
use crate::protocol::{EventId, Id};
use serde_json::json;
use std::collections::BTreeMap;

/// Residual risk always attached to a command the parser did not fully explain.
pub const RESIDUAL_RISK: &str = "Arbitrary programs and scripts are not proven harmless by command inspection; with Full access they run with your user rights.";

#[derive(Debug)]
pub enum ShellAssessment {
    /// Recognized destructive steps to preflight, plus the residual risk of the rest.
    Destructive {
        plans: Vec<EffectPlan>,
        residual: &'static str,
    },
    /// No consequential token at all. Not proof of harmlessness.
    NoRecognizedDestruction { residual: &'static str },
    /// A destructive step whose targets cannot be determined before dispatch, or a
    /// consequential command the analysis did not fully model.
    NeedsInput(InputNeeded),
    /// A forced or deleting ref update of a protected branch: refused like the typed Git command.
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

// ---------------------------------------------------------------------------------------------
// Lexical backstop

/// Token standing for a .NET file-system API call (`[IO.File]::Delete(…)`, `::Move`, …).
const API_TOKEN: &str = "[io]::api";

/// Normalizes a program word: basename, leading backslashes (`\rm`) and `.exe` removed,
/// lowercase.
fn normalize(word: &str) -> String {
    let trimmed = word.trim_start_matches('\\');
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_owned()
}

/// Whether a normalized word names a program or verb that deletes, moves or overwrites.
pub fn consequential_token(name: &str) -> bool {
    matches!(
        name,
        // POSIX
        "rm" | "rmdir" | "unlink" | "shred" | "truncate" | "dd" | "mv" | "cp" | "ln" | "tee"
            | "rsync" | "find" | "git"
            // cmd
            | "rd" | "del" | "erase" | "move" | "ren" | "rename" | "copy"
            // PowerShell
            | "remove-item" | "ri" | "move-item" | "mi" | "rename-item" | "rni" | "set-content"
            | "sc" | "clear-content" | "clc" | "out-file" | "copy-item" | "cpi"
    )
}

/// .NET file-system API calls that delete, move or write.
fn api_calls(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    [
        "::delete",
        "::move",
        "::writeall",
        "::appendall",
        "::replace",
        "::copy",
        "::create",
        "::open",
        "::setlength",
    ]
    .iter()
    .map(|api| lower.matches(api).count())
    .sum()
}

/// Every consequential token in raw text, by normalized name. Quotes, substitutions, blocks
/// and redirections are only separators here, so a token inside any of them is found.
fn lexical(text: &str, out: &mut BTreeMap<String, usize>) {
    for piece in text.split(|c: char| c.is_whitespace() || "\"';|&(){}<>`$,=!@[]".contains(c)) {
        let name = normalize(piece.trim_end_matches(':'));
        if consequential_token(&name) {
            *out.entry(name).or_default() += 1;
        }
    }
    let apis = api_calls(text);
    if apis > 0 {
        *out.entry(API_TOKEN.to_owned()).or_default() += apis;
    }
}

// ---------------------------------------------------------------------------------------------
// Tokenizer

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
    /// `<`, `<<`, `<<<` or `<(` input redirection.
    Input,
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
            let tilde = dialect != Dialect::Cmd && dynamic_tilde(unquoted);
            let array = dialect == Dialect::PowerShell && unquoted.contains(',');
            lx.tokens.push(Token::Word(Word {
                text: std::mem::take(word),
                dynamic: *dynamic || brace || tilde || array,
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
                if let Some(&n) = chars.get(i + 1) {
                    // A POSIX escape before a letter or digit inside a word changes nothing in a
                    // real path but is exactly what a Windows path (`C:\Users\x`) turns into
                    // under `sh`: the word no longer means what was written, so it is held.
                    // (`\rm` at the start of a word, the alias bypass, stays literal.)
                    if dialect == Dialect::Posix && in_word && n.is_ascii_alphanumeric() {
                        dynamic = true;
                    }
                    in_word = true;
                    if !(dialect == Dialect::Posix && n == '\n') {
                        word.push(n);
                    }
                    i += 2;
                } else {
                    in_word = true;
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
            '(' if in_word && dialect != Dialect::Cmd => {
                // `(` adjacent to a word: a zsh glob qualifier, a function definition or a
                // PowerShell method call. Never a plain literal.
                dynamic = true;
                word.push(c);
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
            '{' | '}' if dialect == Dialect::PowerShell => {
                // Script blocks delimit commands; every command inside is checked.
                flush(
                    &mut lx,
                    &mut word,
                    &mut unquoted,
                    &mut in_word,
                    &mut dynamic,
                );
                lx.tokens.push(Token::Group);
            }
            '<' => {
                flush(
                    &mut lx,
                    &mut word,
                    &mut unquoted,
                    &mut in_word,
                    &mut dynamic,
                );
                // `<<`, `<<<`; a following `(` (process substitution) becomes a group.
                while chars.get(i + 1) == Some(&'<') {
                    i += 1;
                }
                lx.tokens.push(Token::Input);
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
/// `~user`, `~+`, `~-` (resolved by the shell at run time); plain `~` and `~/…` are the home
/// directory, which the resolver expands itself.
fn dynamic_tilde(unquoted: &str) -> bool {
    unquoted.starts_with('~')
        && !(unquoted == "~" || unquoted.starts_with("~/") || unquoted.starts_with("~\\"))
}
fn glob(word: &str) -> bool {
    word.contains(['*', '?', '['])
}

// ---------------------------------------------------------------------------------------------
// Structured analysis

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cwd {
    Known(Option<String>),
    /// A working-directory change happened; relative targets cannot be resolved.
    Unknown,
}

struct Collector<'p> {
    plans: Vec<EffectPlan>,
    needs: Vec<InputNeeded>,
    protected: Vec<String>,
    cwd: Cwd,
    protected_branches: &'p [String],
    /// Consequential tokens accounted for by fully modeled commands.
    claimed: BTreeMap<String, usize>,
}
impl Collector<'_> {
    fn hold(&mut self, n: InputNeeded) {
        if !self.needs.contains(&n) {
            self.needs.push(n);
        }
    }
    fn unmodeled(&mut self) {
        self.hold(InputNeeded::UnmodeledConsequentialCommand);
    }
    fn claim(&mut self, name: &str) {
        *self.claimed.entry(name.to_owned()).or_default() += 1;
    }
    fn cwd(&self) -> Option<&str> {
        match &self.cwd {
            Cwd::Known(c) => c.as_deref(),
            Cwd::Unknown => None,
        }
    }
    /// Adds a plan for every operand that resolves (literal, and absolute when the working
    /// directory is unknown) and holds for the rest; returns whether all of them resolved.
    /// Resolvable operands are planned even when others are held, so a step that would be
    /// refused is refused whatever else the command holds.
    fn push(&mut self, kind: DestructiveKind, words: &[Word]) -> bool {
        let mut resolved = Vec::new();
        let mut complete = true;
        for w in words {
            if drive_relative(&w.text) {
                // `C:repo` means "repo in drive C's own current directory", which no saved
                // working directory determines.
                self.hold(InputNeeded::UnresolvedTarget);
                complete = false;
            } else if w.dynamic {
                self.hold(InputNeeded::UnresolvedExpansion);
                complete = false;
            } else if self.cwd == Cwd::Unknown && !absolute(&w.text) {
                self.hold(InputNeeded::UnresolvedWorkingDirectory);
                complete = false;
            } else {
                resolved.push(w.text.clone());
            }
        }
        if !resolved.is_empty() {
            let cwd = self.cwd().map(str::to_owned);
            self.plans.push(plan(kind, resolved, cwd.as_deref()));
        }
        complete
    }
}

/// A drive letter and colon not followed by a separator: relative to that drive's current
/// directory.
fn drive_relative(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && !matches!(b.get(2), Some(b'\\' | b'/'))
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
    assess(argv, working_directory, protected_branches, true)
}

/// Everything the analysis found in one command, without precedence applied: every plan
/// (including those of steps that are also held), every hold and every protected-branch
/// refusal.
#[derive(Debug)]
pub struct ShellAnalysis {
    pub plans: Vec<EffectPlan>,
    pub held: Vec<InputNeeded>,
    pub protected_branches: Vec<String>,
}

/// Runs the structured analysis and the lexical backstop over one argv.
pub fn analyze_command(
    argv: &[String],
    working_directory: Option<&str>,
    protected_branches: &[String],
) -> ShellAnalysis {
    analyze(argv, working_directory, protected_branches, true)
}

fn analyze(
    argv: &[String],
    working_directory: Option<&str>,
    protected_branches: &[String],
    backstop: bool,
) -> ShellAnalysis {
    let words: Vec<Word> = argv.iter().map(|a| literal(a)).collect();
    let mut c = Collector {
        plans: Vec::new(),
        needs: Vec::new(),
        protected: Vec::new(),
        cwd: Cwd::Known(working_directory.map(str::to_owned)),
        protected_branches,
        claimed: BTreeMap::new(),
    };
    command(&words, Dialect::Posix, false, &mut c, 0);
    // The backstop: every consequential token in the raw input must have been accounted for
    // by a fully modeled command.
    let mut found = BTreeMap::new();
    for a in argv {
        lexical(a, &mut found);
    }
    if backstop
        && found
            .iter()
            .any(|(name, n)| c.claimed.get(name).copied().unwrap_or(0) < *n)
    {
        c.unmodeled();
    }
    ShellAnalysis {
        plans: c.plans,
        held: c.needs,
        protected_branches: c.protected,
    }
}

fn assess(
    argv: &[String],
    working_directory: Option<&str>,
    protected_branches: &[String],
    backstop: bool,
) -> ShellAssessment {
    let a = analyze(argv, working_directory, protected_branches, backstop);
    if let Some(target) = a.protected_branches.into_iter().next() {
        return ShellAssessment::ProtectedBranch { target };
    }
    if let Some(n) = a.held.into_iter().next() {
        return ShellAssessment::NeedsInput(n);
    }
    if a.plans.is_empty() {
        ShellAssessment::NoRecognizedDestruction {
            residual: RESIDUAL_RISK,
        }
    } else {
        ShellAssessment::Destructive {
            plans: a.plans,
            residual: RESIDUAL_RISK,
        }
    }
}

/// The structured analysis alone, without the lexical backstop (mutation tests show the
/// backstop is what holds the forms the structured model does not cover).
#[doc(hidden)]
pub fn assess_structured_only(argv: &[String], working_directory: Option<&str>) -> ShellAssessment {
    assess(argv, working_directory, &[], false)
}

// ---------------------------------------------------------------------------------------------
// Verdict across every step: Blocked > NeedsInput > allowed plans > residual

/// One refused step of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocking {
    /// A target the destructive preflight refused (filesystem root, primary repository, Git
    /// metadata, …), with its decision and rationale.
    Target(PreflightDecision),
    /// A forced or deleting ref update of a protected branch.
    ProtectedBranch { target: String },
}
impl Blocking {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Target(d) => match &d.verdict {
                PreflightVerdict::Blocked(r) => r.as_str(),
                _ => "Blocked",
            },
            Self::ProtectedBranch { .. } => "ProtectedBranch",
        }
    }
    pub fn rationale(&self) -> &'static str {
        match self {
            Self::Target(d) => d.rationale(),
            Self::ProtectedBranch { .. } => super::DenialReason::ProtectedBranch.rationale(),
        }
    }
}

/// The verdict on a whole command after every step was preflighted.
///
/// **Precedence:** any step or resolvable target that would be refused makes the command
/// `Blocked`, whatever else it holds; otherwise any hold makes it `NeedsInput`; otherwise its
/// plans are `Allowed`; a command without consequential tokens is `Residual`.
///
/// **Approval never unlocks a refusal.** A `NeedsInput` command may be resolved by a person
/// approving it, but that resolution must re-run [`preflight_command`] on the current state and
/// dispatch only what [`CommandVerdict::dispatchable_after_approval`] returns: a `Blocked`
/// command yields nothing, so deleting a filesystem root, a primary repository or Git metadata
/// stays reserved to the dedicated permit path that no approval or Full access stands in for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandVerdict {
    Blocked {
        blocked: Vec<Blocking>,
        /// Holds found as well (secondary information).
        held: Vec<InputNeeded>,
    },
    NeedsInput {
        held: Vec<InputNeeded>,
        /// Steps that would be allowed once the holds are resolved.
        allowed: Vec<PreflightDecision>,
    },
    Allowed {
        decisions: Vec<PreflightDecision>,
    },
    Residual {
        residual: &'static str,
    },
}
impl CommandVerdict {
    /// The preflight decisions an approval of this verdict may dispatch: the allowed steps of a
    /// held command (or of an allowed one); nothing for a blocked command. A caller must obtain
    /// this from a fresh [`preflight_command`] at approval time, never from a stored verdict.
    pub fn dispatchable_after_approval(&self) -> Option<&[PreflightDecision]> {
        match self {
            Self::Blocked { .. } | Self::Residual { .. } => None,
            Self::NeedsInput { allowed, .. } => Some(allowed),
            Self::Allowed { decisions } => Some(decisions),
        }
    }
}

/// Analyzes a command and preflights every one of its steps, applying the precedence above.
pub fn preflight_command(
    fs: &dyn FilesystemResolver,
    roots: &ProtectedRoots,
    argv: &[String],
    working_directory: Option<&str>,
    protected_branches: &[String],
) -> CommandVerdict {
    let analysis = analyze_command(argv, working_directory, protected_branches);
    let mut blocked: Vec<Blocking> = analysis
        .protected_branches
        .into_iter()
        .map(|target| Blocking::ProtectedBranch { target })
        .collect();
    let mut held = analysis.held;
    let mut allowed = Vec::new();
    for p in &analysis.plans {
        let decision = preflight(fs, roots, p);
        match decision.verdict.clone() {
            PreflightVerdict::Blocked(_) => blocked.push(Blocking::Target(decision)),
            PreflightVerdict::NeedsInput(n) => {
                if !held.contains(&n) {
                    held.push(n);
                }
            }
            PreflightVerdict::Allowed => allowed.push(decision),
        }
    }
    if !blocked.is_empty() {
        CommandVerdict::Blocked { blocked, held }
    } else if !held.is_empty() {
        CommandVerdict::NeedsInput { held, allowed }
    } else if !allowed.is_empty() {
        CommandVerdict::Allowed { decisions: allowed }
    } else {
        CommandVerdict::Residual {
            residual: RESIDUAL_RISK,
        }
    }
}

/// Records a blocked or held command verdict in history, attributed to the invoking principal,
/// with every blocked reason and rationale and the holds as secondary information.
pub fn record_verdict(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &str,
    argv: &[String],
    verdict: &CommandVerdict,
) -> Result<Option<i64>, StorageError> {
    let (kind, blocked, held) = match verdict {
        CommandVerdict::Blocked { blocked, held } => {
            ("DestructiveOperationBlocked", &blocked[..], &held[..])
        }
        CommandVerdict::NeedsInput { held, .. } => ("DestructiveOperationHeld", &[][..], &held[..]),
        _ => return Ok(None),
    };
    let digest = sha256_hex(argv.join("\u{0}").as_bytes());
    let reasons: Vec<serde_json::Value> = blocked
        .iter()
        .map(|b| {
            json!({
                "reason": b.reason(),
                "rationale": b.rationale(),
                "target": match b {
                    Blocking::Target(d) => d.offending.clone(),
                    Blocking::ProtectedBranch { target } => Some(target.clone()),
                },
            })
        })
        .collect();
    let held: Vec<&str> = held.iter().map(|n| n.as_str()).collect();
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
                "blocked": reasons,
                "held": held,
                "program": argv.first().map(|p| Redactor::new().redact_text(&normalize(p))),
                "argvSha256": digest,
                "principalKind": format!("{:?}", ctx.principal_kind()),
                "authoritySources": sources,
            }),
        })
        .map(Some)
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
    if dialect == Dialect::PowerShell {
        io_api(text, c);
    }
    let mut current: Vec<Word> = Vec::new();
    let mut redirect = None;
    // Whether the command being collected reads a pipeline, whether the last separator was a
    // pipe, and whether it reads redirected input (`<`, `<<`, `<<<`, `<(`).
    let mut piped = false;
    let mut after_pipe = false;
    let mut fed = false;
    let run = |current: &mut Vec<Word>, piped: bool, fed: bool, c: &mut Collector| {
        if fed
            && current
                .first()
                .is_some_and(|w| shell_program(&normalize(&w.text)))
        {
            // A shell reading its script from a heredoc, here-string or process substitution.
            c.hold(InputNeeded::OpaqueCommand);
        }
        command(current, dialect, piped, c, depth + 1);
        current.clear();
    };
    for t in lexed.tokens {
        match t {
            Token::Separator { pipe } => {
                run(&mut current, piped, fed, c);
                fed = false;
                after_pipe = pipe;
            }
            // Group boundaries delimit commands; every grouped command is checked.
            Token::Group => {
                run(&mut current, piped, fed, c);
                fed = false;
            }
            Token::Input => fed = true,
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
    run(&mut current, piped, fed, c);
}

/// `[IO.Directory]::Delete('<literal>' …)` and friends with a literal first argument become a
/// consequential plan (refused for protected roots, held otherwise); anything else is left to
/// the backstop.
fn io_api(text: &str, c: &mut Collector) {
    let lower = text.to_ascii_lowercase();
    for api in ["::delete(", "::move("] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(api) {
            let start = from + found + api.len();
            from = start;
            let rest = text[start..].trim_start();
            let Some(quote) = rest.chars().next().filter(|q| *q == '\'' || *q == '"') else {
                continue;
            };
            let Some(end) = rest[1..].find(quote) else {
                continue;
            };
            let path = &rest[1..1 + end];
            if path.contains('$') {
                continue;
            }
            if c.push(DestructiveKind::Consequential, &[literal(path)]) {
                c.claim(API_TOKEN);
            }
        }
    }
}

fn shell_program(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "ksh"
            | "fish"
            | "ash"
            | "csh"
            | "tcsh"
            | "cmd"
            | "powershell"
            | "pwsh"
    )
}
fn interpreter(name: &str) -> bool {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    matches!(
        base,
        "perl" | "python" | "ruby" | "node" | "nodejs" | "deno" | "php"
    )
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
    )
}

/// Option grammar of one program: short letters taking a value, short flag letters, long
/// options taking a value, long flags, and options that mean "no execution".
struct Options<'a> {
    value_letters: &'a str,
    flag_letters: &'a str,
    long_values: &'a [&'a str],
    long_flags: &'a [&'a str],
    stop: &'a [&'a str],
}

enum Parsed {
    /// Index of the first operand.
    Operand(usize),
    /// An option meaning "do not execute" (`sudo -l`, `command -v`).
    NoExecution,
    /// An option the model does not know.
    Unknown,
}

/// Parses options from `i` (combined short options included); `seen` receives every option
/// with its value.
fn parse_options(
    words: &[Word],
    mut i: usize,
    o: &Options<'_>,
    seen: &mut Vec<(String, Option<String>)>,
) -> Parsed {
    while let Some(w) = words.get(i) {
        let t = w.text.as_str();
        if w.dynamic && t.starts_with('-') {
            return Parsed::Unknown;
        }
        if t == "--" {
            return Parsed::Operand(i + 1);
        }
        if !t.starts_with('-') || t == "-" {
            return Parsed::Operand(i);
        }
        if o.stop.contains(&t) {
            return Parsed::NoExecution;
        }
        if let Some(long) = t.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            let name = format!("--{name}");
            if o.stop.contains(&name.as_str()) {
                return Parsed::NoExecution;
            }
            let (valued, flag) = (
                o.long_values.contains(&name.as_str()),
                o.long_flags.contains(&name.as_str()),
            );
            match value {
                Some(v) if valued => {
                    seen.push((name, Some(v)));
                    i += 1;
                }
                None if flag => {
                    seen.push((name, None));
                    i += 1;
                }
                None if valued => {
                    seen.push((name, words.get(i + 1).map(|w| w.text.clone())));
                    i += 2;
                }
                _ => return Parsed::Unknown,
            }
            continue;
        }
        // A short option cluster.
        let letters: Vec<char> = t[1..].chars().collect();
        let mut k = 0;
        let mut consumed = 1;
        while k < letters.len() {
            let l = letters[k];
            let opt = format!("-{l}");
            if o.stop.contains(&opt.as_str()) {
                return Parsed::NoExecution;
            }
            if o.value_letters.contains(l) {
                let rest: String = letters[k + 1..].iter().collect();
                if rest.is_empty() {
                    seen.push((opt, words.get(i + 1).map(|w| w.text.clone())));
                    consumed = 2;
                } else {
                    seen.push((opt, Some(rest)));
                }
                break;
            }
            if !o.flag_letters.contains(l) {
                return Parsed::Unknown;
            }
            seen.push((opt, None));
            k += 1;
        }
        i += consumed;
    }
    Parsed::Operand(i)
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
    let mut git_env = false;
    loop {
        let Some(w) = words.get(i) else { return };
        let t = w.text.as_str();
        if dialect == Dialect::Posix && transparent_keyword(t) {
            i += 1;
            continue;
        }
        if dialect == Dialect::PowerShell
            && matches!(
                t.to_ascii_lowercase().as_str(),
                "if" | "elseif"
                    | "else"
                    | "try"
                    | "catch"
                    | "finally"
                    | "while"
                    | "do"
                    | "until"
                    | "foreach-object"
                    | "%"
                    | "where-object"
                    | "?"
            )
        {
            // Keywords and block-taking cmdlets whose block was split off as its own command.
            i += 1;
            continue;
        }
        if dialect == Dialect::PowerShell
            && matches!(
                t.to_ascii_lowercase().as_str(),
                "foreach" | "for" | "switch"
            )
        {
            // The loop header executes nothing destructive; the block is checked separately.
            return;
        }
        if dialect == Dialect::Posix && t == "time" {
            i += 1;
            if words.get(i).is_some_and(|w| w.text == "-p") {
                i += 1;
            }
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
            git_env |= t.starts_with("GIT_");
            i += 1;
            continue;
        }
        let name = normalize(t);
        let mut seen = Vec::new();
        let parsed = match name.as_str() {
            "env" => {
                let p = parse_options(
                    words,
                    i + 1,
                    &Options {
                        value_letters: "uCS",
                        flag_letters: "i0v",
                        long_values: &["--unset", "--chdir", "--split-string"],
                        long_flags: &["--ignore-environment", "--null", "--debug"],
                        stop: &[],
                    },
                    &mut seen,
                );
                if seen.iter().any(|(o, _)| o == "-C" || o == "--chdir") {
                    c.cwd = Cwd::Unknown;
                }
                if seen.iter().any(|(o, _)| o == "-S" || o == "--split-string") {
                    c.hold(InputNeeded::OpaqueCommand);
                    return;
                }
                // Assignments after the options.
                match p {
                    Parsed::Operand(mut j) => {
                        while words
                            .get(j)
                            .is_some_and(|w| w.text.contains('=') && !w.text.starts_with('-'))
                        {
                            git_env |= words[j].text.starts_with("GIT_");
                            j += 1;
                        }
                        Parsed::Operand(j)
                    }
                    other => other,
                }
            }
            "sudo" => {
                let p = parse_options(
                    words,
                    i + 1,
                    &Options {
                        value_letters: "ugCDhprtTUR",
                        flag_letters: "AbEHiknPSsBN",
                        long_values: &[
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
                            "--chroot",
                            "--preserve-env",
                        ],
                        long_flags: &[
                            "--askpass",
                            "--background",
                            "--preserve-env",
                            "--set-home",
                            "--login",
                            "--non-interactive",
                            "--shell",
                            "--stdin",
                            "--reset-timestamp",
                        ],
                        stop: &[
                            "-e",
                            "--edit",
                            "-l",
                            "--list",
                            "-v",
                            "--validate",
                            "-K",
                            "-V",
                        ],
                    },
                    &mut seen,
                );
                if seen
                    .iter()
                    .any(|(o, _)| matches!(o.as_str(), "-D" | "--chdir" | "-R" | "--chroot"))
                {
                    c.cwd = Cwd::Unknown;
                }
                p
            }
            "doas" => parse_options(
                words,
                i + 1,
                &Options {
                    value_letters: "uC",
                    flag_letters: "ns",
                    long_values: &[],
                    long_flags: &[],
                    stop: &["-C"],
                },
                &mut seen,
            ),
            "timeout" => match parse_options(
                words,
                i + 1,
                &Options {
                    value_letters: "sk",
                    flag_letters: "v",
                    long_values: &["--signal", "--kill-after"],
                    long_flags: &["--foreground", "--preserve-status", "--verbose"],
                    stop: &[],
                },
                &mut seen,
            ) {
                // The duration operand precedes the command.
                Parsed::Operand(j) => Parsed::Operand(j + 1),
                other => other,
            },
            "nice" => {
                // `nice -10 cmd` (obsolete numeric form).
                let numeric = words.get(i + 1).is_some_and(|w| {
                    w.text.len() > 1
                        && w.text[1..].chars().all(|d| d.is_ascii_digit())
                        && w.text.starts_with('-')
                });
                if numeric {
                    Parsed::Operand(i + 2)
                } else {
                    parse_options(
                        words,
                        i + 1,
                        &Options {
                            value_letters: "n",
                            flag_letters: "",
                            long_values: &["--adjustment"],
                            long_flags: &[],
                            stop: &[],
                        },
                        &mut seen,
                    )
                }
            }
            "nohup" | "builtin" => parse_options(
                words,
                i + 1,
                &Options {
                    value_letters: "",
                    flag_letters: "",
                    long_values: &[],
                    long_flags: &[],
                    stop: &[],
                },
                &mut seen,
            ),
            "command" => parse_options(
                words,
                i + 1,
                &Options {
                    value_letters: "",
                    flag_letters: "p",
                    long_values: &[],
                    long_flags: &[],
                    stop: &["-v", "-V"],
                },
                &mut seen,
            ),
            "exec" => parse_options(
                words,
                i + 1,
                &Options {
                    value_letters: "a",
                    flag_letters: "cl",
                    long_values: &[],
                    long_flags: &[],
                    stop: &[],
                },
                &mut seen,
            ),
            "xargs" => {
                let p = parse_options(
                    words,
                    i + 1,
                    &Options {
                        value_letters: "adEeIiLlnPs",
                        flag_letters: "0rtpx",
                        long_values: &[
                            "--arg-file",
                            "--delimiter",
                            "--max-args",
                            "--max-procs",
                            "--max-chars",
                            "--max-lines",
                            "--replace",
                            "--eof",
                        ],
                        long_flags: &[
                            "--null",
                            "--no-run-if-empty",
                            "--verbose",
                            "--interactive",
                            "--exit",
                        ],
                        stop: &[],
                    },
                    &mut seen,
                );
                // Targets arrive on standard input.
                if let Parsed::Operand(j) = p
                    && let Some(w) = words.get(j)
                {
                    let inner = normalize(&w.text);
                    if consequential_token(&inner) || shell_program(&inner) || interpreter(&inner) {
                        c.hold(InputNeeded::UnresolvedTarget);
                    }
                }
                return;
            }
            _ => {
                if dialect == Dialect::PowerShell && t.contains("::") {
                    // A .NET static call: modeled (or held) by the API scan of the script,
                    // and any API left unmodeled is held by the backstop.
                    return;
                }
                if words[i].dynamic {
                    // The program itself is computed at run time.
                    c.hold(InputNeeded::UnresolvedExpansion);
                    return;
                }
                if glob(t) && t != "[" && t != "[[" {
                    // `/bin/r?`: the program is chosen by a glob.
                    c.hold(InputNeeded::UnmodeledConsequentialCommand);
                    return;
                }
                break;
            }
        };
        match parsed {
            Parsed::Operand(j) => i = j,
            Parsed::NoExecution => return,
            Parsed::Unknown => {
                // An unknown wrapper option: the wrapped program cannot be identified.
                if words[i..]
                    .iter()
                    .any(|w| consequential_token(&normalize(&w.text)))
                {
                    c.unmodeled();
                }
                return;
            }
        }
    }
    let head = &words[i];
    let name = normalize(&head.text);
    let args = &words[i + 1..];
    let modeled = match name.as_str() {
        n if shell_program(n) && n != "cmd" && n != "powershell" && n != "pwsh" => {
            shell(&name, args, piped, c, depth);
            return;
        }
        "cmd" => {
            cmd(args, piped, c, depth);
            return;
        }
        "powershell" | "pwsh" => {
            powershell(&name, args, piped, c, depth);
            return;
        }
        n if interpreter(n) => {
            inline_code(args, c);
            return;
        }
        "cd" | "chdir" | "pushd" | "popd" | "set-location" | "sl" | "push-location"
        | "pop-location" => {
            c.cwd = Cwd::Unknown;
            return;
        }
        "find" => find(args, c),
        "git" => !git_env && git(args, c),
        "mv" if dialect != Dialect::PowerShell => mv(args, c),
        "move-item" | "mi" | "mv" | "move" if dialect == Dialect::PowerShell => move_item(args, c),
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
            args.iter()
                .all(|a| a.text.contains('=') && !a.text.starts_with('-'))
                && (outputs.is_empty() || c.push(DestructiveKind::Truncate, &outputs))
        }
        "rm" | "rmdir" | "unlink" | "shred" if dialect == Dialect::Posix => {
            posix_removal(&name, args, c)
        }
        "rd" | "rmdir" | "del" | "erase" if dialect != Dialect::PowerShell => {
            cmd_removal(&name, args, c)
        }
        "remove-item" | "ri" | "rm" | "rd" | "del" | "erase" | "rmdir"
            if dialect == Dialect::PowerShell =>
        {
            remove_item(args, piped, c)
        }
        "remove-item" | "ri" => remove_item(args, piped, c),
        _ => false,
    };
    if modeled {
        c.claim(&name);
    } else if consequential_token(&name) {
        c.unmodeled();
    }
}

fn shell(name: &str, args: &[Word], piped: bool, c: &mut Collector, depth: u8) {
    // Options first; `-c` (in any cluster) or fish/csh `--command` takes the script from the
    // first operand after the options.
    let mut j = 0;
    let mut inline = false;
    while let Some(a) = args.get(j) {
        let t = a.text.as_str();
        if t == "--" || t == "-" {
            j += 1;
            break;
        }
        if t == "-o" || t == "+o" || t == "-O" || t == "+O" || t == "--rcfile" || t == "--init-file"
        {
            j += 2;
            continue;
        }
        if t == "--command" || (name == "fish" && t == "-C") {
            inline = true;
            j += 1;
            break;
        }
        if let Some(body) = t.strip_prefix("--command=") {
            script(body, Dialect::Posix, c, depth + 1);
            return;
        }
        if t.starts_with('-') || t.starts_with('+') {
            inline |= !t.starts_with("--") && t[1..].contains('c');
            j += 1;
            continue;
        }
        break;
    }
    if inline {
        match args.get(j) {
            Some(body) if body.dynamic => c.hold(InputNeeded::UnresolvedExpansion),
            Some(body) => script(&body.text, Dialect::Posix, c, depth + 1),
            None => c.hold(InputNeeded::OpaqueCommand),
        }
        return;
    }
    if args.get(j).is_none() && piped {
        // A script read from a pipeline cannot be inspected before dispatch.
        c.hold(InputNeeded::OpaqueCommand);
    }
    // Otherwise a script file: opaque, reported as residual risk unless the backstop sees a
    // consequential token in the argv.
}

fn cmd(args: &[Word], piped: bool, c: &mut Collector, depth: u8) {
    for (k, a) in args.iter().enumerate() {
        let l = a.text.to_ascii_lowercase();
        let Some(rest) = ["/c", "/k", "/r"].iter().find_map(|p| l.strip_prefix(p)) else {
            if l.starts_with('/') {
                continue;
            }
            break;
        };
        // `/c` may be joined to the command (`cmd /crd /s /q x`).
        let mut body: Vec<&str> = Vec::new();
        if !rest.is_empty() {
            body.push(&a.text[2..]);
        }
        body.extend(args[k + 1..].iter().map(|a| a.text.as_str()));
        script(&body.join(" "), Dialect::Cmd, c, depth + 1);
        return;
    }
    if piped {
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
        let command_text = opt == "cwa"
            || opt == "commandwithargs"
            || (!opt.is_empty() && "command".starts_with(opt));
        if command_text {
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

/// Inline interpreter code that names a deletion, move or write API is held.
fn inline_code(args: &[Word], c: &mut Collector) {
    let Some(k) = args.iter().position(|a| {
        matches!(a.text.as_str(), "-e" | "-E" | "-c" | "--eval" | "-p" | "-r")
            || (a.text.starts_with('-') && !a.text.starts_with("--") && a.text.ends_with('e'))
    }) else {
        return;
    };
    let Some(code) = args.get(k + 1) else {
        return;
    };
    let lower = code.text.to_ascii_lowercase();
    const APIS: [&str; 16] = [
        "unlink",
        "rmtree",
        "remove",
        "rmdir",
        "rm_rf",
        "rm_r",
        "remove-item",
        "delete",
        "rename",
        "truncate",
        "move",
        "rimraf",
        "rmsync",
        ".rm(",
        "fs.rm",
        "writefile",
    ];
    if code.dynamic || APIS.iter().any(|api| lower.contains(api)) {
        c.unmodeled();
    }
}

fn find(args: &[Word], c: &mut Collector) -> bool {
    let mut j = 0;
    // Global options before the starting points.
    while let Some(a) = args.get(j) {
        match a.text.as_str() {
            "-L" | "-follow" => {
                // Deletion would follow links into their targets.
                c.hold(InputNeeded::UnresolvedTarget);
                return false;
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
    const UNARY: [&str; 27] = [
        "-name",
        "-iname",
        "-path",
        "-ipath",
        "-wholename",
        "-iwholename",
        "-type",
        "-xtype",
        "-mtime",
        "-mmin",
        "-atime",
        "-amin",
        "-ctime",
        "-cmin",
        "-newer",
        "-size",
        "-perm",
        "-user",
        "-group",
        "-maxdepth",
        "-mindepth",
        "-regex",
        "-iregex",
        "-links",
        "-inum",
        "-samefile",
        "-fstype",
    ];
    const NULLARY: [&str; 18] = [
        "-print", "-print0", "-delete", "-depth", "-prune", "-empty", "-xdev", "-mount", "-o",
        "-a", "-or", "-and", "-not", "!", "(", ")", "-true", "-false",
    ];
    let mut deletes = false;
    let mut k = 0;
    while k < expr.len() {
        let t = expr[k].text.as_str();
        if matches!(t, "-exec" | "-execdir" | "-ok" | "-okdir") {
            let inner: Vec<Word> = expr[k + 1..]
                .iter()
                .take_while(|a| a.text != ";" && a.text != "+")
                .cloned()
                .collect();
            let Some(p) = inner.first() else {
                return false;
            };
            let n = normalize(&p.text);
            match n.as_str() {
                "rm" | "rmdir" | "unlink" | "shred" => {
                    deletes = true;
                    c.claim(&n);
                }
                _ if consequential_token(&n) || shell_program(&n) || interpreter(&n) => {
                    c.hold(InputNeeded::OpaqueCommand);
                    return false;
                }
                _ => {}
            }
            k += inner.len() + 2;
            continue;
        }
        if t == "-follow" {
            c.hold(InputNeeded::UnresolvedTarget);
            return false;
        }
        if t == "-delete" {
            deletes = true;
        }
        if UNARY.contains(&t) {
            k += 2;
        } else if NULLARY.contains(&t) {
            k += 1;
        } else {
            // An expression primary this model does not know.
            return false;
        }
    }
    if deletes {
        let starts = if starts.is_empty() {
            vec![literal(".")]
        } else {
            starts
        };
        return c.push(DestructiveKind::EraseContents, &starts);
    }
    true
}

fn protected_branch(c: &Collector, dest: &str) -> bool {
    protected_ref(dest, c.protected_branches)
}

/// The directory a Git command works in, as a target word.
fn git_dir(dir: &Option<Word>) -> Word {
    dir.clone().unwrap_or_else(|| literal("."))
}

fn git(args: &[Word], c: &mut Collector) -> bool {
    let mut j = 0;
    let mut dir: Option<Word> = None;
    while let Some(a) = args.get(j) {
        match a.text.as_str() {
            "-C" => {
                if dir.is_some() {
                    // Stacked `-C` paths compose; hold rather than join.
                    c.hold(InputNeeded::UnresolvedTarget);
                    return false;
                }
                dir = args.get(j + 1).cloned();
                j += 2;
            }
            "-c" => {
                let key = args.get(j + 1).map(|v| v.text.to_ascii_lowercase());
                if key.is_none_or(|k| k.starts_with("alias.") || k.starts_with("core.")) {
                    // An alias or core setting can change what the subcommand does.
                    return false;
                }
                j += 2;
            }
            "--no-pager"
            | "-p"
            | "--paginate"
            | "-P"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--no-optional-locks" => j += 1,
            // Repository relocation (`--git-dir`, `--work-tree`, …) is not modeled.
            t if t.starts_with('-') => return false,
            _ => break,
        }
    }
    let Some(sub) = args.get(j) else {
        return true;
    };
    let rest = &args[j + 1..];
    let consequential = |c: &mut Collector, dir: &Option<Word>| {
        c.push(DestructiveKind::Consequential, &[git_dir(dir)])
    };
    match sub.text.as_str() {
        "status" | "log" | "show" | "diff" | "fetch" | "add" | "commit" | "init" | "describe"
        | "blame" | "ls-files" | "ls-tree" | "rev-parse" | "rev-list" | "shortlog" | "grep"
        | "remote" | "tag" | "merge" | "pull" | "rebase" | "cherry-pick" | "revert" | "am"
        | "config" | "help" | "version" | "show-ref" | "for-each-ref" | "cat-file" => true,
        "stash" => match rest.first().map(|w| w.text.as_str()) {
            Some("drop" | "clear") => consequential(c, &dir),
            _ => true,
        },
        "checkout" | "switch" => {
            let forced = rest.iter().any(|a| {
                matches!(
                    a.text.as_str(),
                    "-f" | "--force" | "--discard-changes" | "--" | "." | "-B"
                )
            });
            !forced || consequential(c, &dir)
        }
        "restore" => consequential(c, &dir),
        "rm" => {
            // `rm` here is the Git subcommand word, accounted for by this model.
            c.claim("rm");
            consequential(c, &dir)
        }
        "reset" => {
            let hard = rest
                .iter()
                .any(|a| matches!(a.text.as_str(), "--hard" | "--merge" | "--keep"));
            !hard || consequential(c, &dir)
        }
        "gc" | "prune" | "reflog" | "repack" => consequential(c, &dir),
        "update-ref" => {
            let delete = rest.iter().any(|a| a.text == "-d" || a.text == "--delete");
            let target = rest.iter().find(|a| !a.text.starts_with('-'));
            if let Some(t) = target
                && protected_branch(c, &t.text)
            {
                c.protected.push(t.text.clone());
                return true;
            }
            if delete || target.is_some() {
                consequential(c, &dir)
            } else {
                true
            }
        }
        "branch" => {
            let destructive = rest.iter().any(|a| {
                matches!(
                    a.text.as_str(),
                    "-D" | "-d" | "--delete" | "-f" | "--force" | "-M" | "-m" | "--move"
                )
            });
            if !destructive {
                return true;
            }
            if let Some(p) = rest
                .iter()
                .find(|a| !a.text.starts_with('-') && protected_branch(c, &a.text))
            {
                c.protected.push(p.text.clone());
                return true;
            }
            consequential(c, &dir)
        }
        "clean" => {
            let mut seen = Vec::new();
            let parsed = parse_options(
                rest,
                0,
                &Options {
                    value_letters: "e",
                    flag_letters: "dfxXnqi",
                    long_values: &["--exclude"],
                    long_flags: &["--force", "--dry-run", "--quiet", "--interactive"],
                    stop: &["-n", "--dry-run"],
                },
                &mut seen,
            );
            let k = match parsed {
                Parsed::Operand(k) => k,
                Parsed::NoExecution => return true,
                Parsed::Unknown => return false,
            };
            let paths = &rest[k..];
            if paths.iter().any(|p| p.text.starts_with('-')) {
                return false;
            }
            let base = git_dir(&dir);
            let targets = if paths.is_empty() {
                vec![base]
            } else if dir.is_none() {
                paths.to_vec()
            } else {
                // Pathspecs relative to `-C`: hold rather than guess the join.
                c.hold(InputNeeded::UnresolvedTarget);
                return false;
            };
            c.push(DestructiveKind::EraseContents, &targets)
        }
        "worktree" => match rest.first().map(|w| w.text.as_str()) {
            Some("remove") => {
                let paths: Vec<Word> = rest[1..]
                    .iter()
                    .filter(|a| !a.text.starts_with('-'))
                    .cloned()
                    .collect();
                if dir.is_some() && paths.iter().any(|p| !absolute(&p.text)) {
                    c.hold(InputNeeded::UnresolvedTarget);
                    return false;
                }
                c.push(DestructiveKind::RemoveTree, &paths)
            }
            Some("list" | "add" | "prune" | "lock" | "unlock") => true,
            _ => false,
        },
        "push" => {
            push(rest, c);
            true
        }
        _ => false,
    }
}

fn push(rest: &[Word], c: &mut Collector) {
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
        let bare = t.trim_start_matches('+');
        let (src, dest) = match bare.split_once(':') {
            Some((src, dest)) => (src, dest),
            None => (bare, bare),
        };
        if src.is_empty() {
            rewrites = true;
        }
        if (forced || force || delete || src.is_empty()) && protected_branch(c, dest) {
            c.protected.push(dest.to_owned());
            continue;
        }
        rewrites |= forced;
        if r.dynamic || glob(t) {
            // A wildcard or computed refspec could name a protected branch.
            c.hold(InputNeeded::UnresolvedTarget);
        }
    }
    let current_branch = |r: &Word| {
        let bare = r.text.trim_start_matches('+');
        let src = bare.split(':').next().unwrap_or(bare);
        src == "HEAD" || src == "@" || bare.ends_with(":HEAD") || bare.ends_with(":@")
    };
    if rewrites && (refspecs.is_empty() || broad || refspecs.iter().any(current_branch)) {
        // The rewritten destination cannot be determined before dispatch.
        c.hold(InputNeeded::UnresolvedTarget);
    }
}

fn mv(args: &[Word], c: &mut Collector) -> bool {
    let mut seen = Vec::new();
    let Parsed::Operand(k) = parse_options(
        args,
        0,
        &Options {
            value_letters: "tS",
            flag_letters: "finuvbT",
            long_values: &["--target-directory", "--suffix", "--backup"],
            long_flags: &[
                "--force",
                "--interactive",
                "--no-clobber",
                "--update",
                "--verbose",
                "--no-target-directory",
                "--strip-trailing-slashes",
                "--backup",
                "--exchange",
            ],
            stop: &[],
        },
        &mut seen,
    ) else {
        return false;
    };
    let operands = &args[k..];
    if operands.iter().any(|o| o.text.starts_with('-')) {
        return false;
    }
    let target_dir = seen
        .iter()
        .find(|(o, _)| o == "-t" || o == "--target-directory")
        .and_then(|(_, v)| v.clone());
    let (sources, destination) = match target_dir {
        Some(dir) => (operands.to_vec(), literal(&dir)),
        None => match operands.split_last() {
            Some((dest, sources)) if !sources.is_empty() => (sources.to_vec(), dest.clone()),
            _ => return false,
        },
    };
    // The sources leave their location, and the destination may be overwritten.
    c.push(DestructiveKind::Move, &sources) && c.push(DestructiveKind::Truncate, &[destination])
}

fn move_item(args: &[Word], c: &mut Collector) -> bool {
    let mut sources = Vec::new();
    let mut destination = None;
    let mut j = 0;
    while let Some(a) = args.get(j) {
        let l = a.text.to_ascii_lowercase();
        match l.as_str() {
            "-path" | "-literalpath" => {
                sources.extend(args.get(j + 1).cloned());
                j += 2;
            }
            "-destination" => {
                destination = args.get(j + 1).cloned();
                j += 2;
            }
            "-force" | "-confirm" | "-whatif" | "-passthru" => j += 1,
            l if l.starts_with('-') => return false,
            _ => {
                if sources.is_empty() {
                    sources.push(a.clone());
                } else if destination.is_none() {
                    destination = Some(a.clone());
                } else {
                    return false;
                }
                j += 1;
            }
        }
    }
    let Some(destination) = destination else {
        return false;
    };
    !sources.is_empty()
        && c.push(DestructiveKind::Move, &sources)
        && c.push(DestructiveKind::Truncate, &[destination])
}

fn remote_path(p: &str) -> bool {
    p.starts_with("rsync://")
        || p.split_once(':')
            .is_some_and(|(host, _)| host.len() > 1 && !host.contains(['/', '\\']))
}

fn rsync(args: &[Word], c: &mut Collector) -> bool {
    let mut seen = Vec::new();
    let Parsed::Operand(_) = parse_options(
        args,
        0,
        &Options {
            value_letters: "e",
            flag_letters: "avrzhPqcunlpgotD",
            long_values: &["--exclude", "--include", "--info", "--rsh", "--filter"],
            long_flags: &[
                "--archive",
                "--verbose",
                "--recursive",
                "--compress",
                "--human-readable",
                "--progress",
                "--partial",
                "--quiet",
                "--checksum",
                "--update",
                "--dry-run",
                "--delete",
                "--delete-before",
                "--delete-during",
                "--delete-after",
                "--delete-delay",
                "--delete-excluded",
                "--remove-source-files",
                "--stats",
                "--itemize-changes",
                "--links",
                "--perms",
                "--times",
                "--group",
                "--owner",
            ],
            stop: &["-n", "--dry-run"],
        },
        &mut seen,
    ) else {
        return false;
    };
    let operands: Vec<Word> = args
        .iter()
        .filter(|a| !a.text.starts_with('-'))
        .cloned()
        .collect();
    let Some((dest, sources)) = operands.split_last() else {
        return true;
    };
    let delete = seen.iter().any(|(o, _)| o.starts_with("--delete"));
    let mut ok = true;
    if !remote_path(&dest.text) {
        // A delete pass erases the destination's extra entries; any transfer may overwrite.
        let kind = if delete {
            DestructiveKind::EraseContents
        } else {
            DestructiveKind::Consequential
        };
        ok &= c.push(kind, std::slice::from_ref(dest));
    }
    if seen.iter().any(|(o, _)| o == "--remove-source-files") {
        let local: Vec<Word> = sources
            .iter()
            .filter(|s| !remote_path(&s.text))
            .cloned()
            .collect();
        if !local.is_empty() {
            ok &= c.push(DestructiveKind::EraseContents, &local);
        }
    }
    ok
}

fn posix_removal(name: &str, args: &[Word], c: &mut Collector) -> bool {
    let options = match name {
        "rm" => Options {
            value_letters: "",
            flag_letters: "rRfidIv",
            long_values: &["--interactive", "--preserve-root"],
            long_flags: &[
                "--recursive",
                "--force",
                "--dir",
                "--verbose",
                "--one-file-system",
                "--no-preserve-root",
                "--preserve-root",
                "--interactive",
            ],
            stop: &[],
        },
        "rmdir" => Options {
            value_letters: "",
            flag_letters: "pv",
            long_values: &[],
            long_flags: &["--parents", "--verbose", "--ignore-fail-on-non-empty"],
            stop: &[],
        },
        "shred" => Options {
            value_letters: "ns",
            flag_letters: "fuvxz",
            long_values: &["--iterations", "--size", "--random-source", "--remove"],
            long_flags: &["--force", "--remove", "--verbose", "--exact", "--zero"],
            stop: &[],
        },
        _ => Options {
            value_letters: "",
            flag_letters: "",
            long_values: &[],
            long_flags: &[],
            stop: &[],
        },
    };
    let mut seen = Vec::new();
    let Parsed::Operand(k) = parse_options(args, 0, &options, &mut seen) else {
        return false;
    };
    let operands = &args[k..];
    if operands.is_empty() {
        return true;
    }
    let recursive = seen
        .iter()
        .any(|(o, _)| matches!(o.as_str(), "-r" | "-R" | "--recursive"));
    let kind = match name {
        "shred" => DestructiveKind::Truncate,
        "rm" if recursive => DestructiveKind::RemoveTree,
        _ => DestructiveKind::Unlink,
    };
    c.push(kind, operands)
}

/// cmd `rd`/`rmdir`/`del`/`erase`, switches separate (`/s /q`) or joined (`/s/q`, `/f/s/q`).
fn cmd_removal(name: &str, args: &[Word], c: &mut Collector) -> bool {
    let known: &[&str] = if matches!(name, "rd" | "rmdir") {
        &["s", "q"]
    } else {
        &["p", "f", "s", "q", "a"]
    };
    let mut recursive = false;
    let mut operands = Vec::new();
    for a in args {
        if a.text.starts_with('/') {
            for switch in a.text.split('/').filter(|s| !s.is_empty()) {
                let s = switch.to_ascii_lowercase();
                let base = s.split(':').next().unwrap_or(&s);
                if !known.contains(&base) {
                    return false;
                }
                recursive |= base == "s";
            }
        } else {
            operands.push(a.clone());
        }
    }
    if operands.is_empty() {
        return true;
    }
    let kind = match (name, recursive) {
        ("del" | "erase", true) => DestructiveKind::EraseContents,
        (_, true) => DestructiveKind::RemoveTree,
        _ => DestructiveKind::Unlink,
    };
    c.push(kind, &operands)
}

/// PowerShell `Remove-Item` and its aliases.
fn remove_item(args: &[Word], piped: bool, c: &mut Collector) -> bool {
    let mut recursive = false;
    let mut operands = Vec::new();
    let mut j = 0;
    while let Some(a) = args.get(j) {
        let l = a.text.to_ascii_lowercase();
        let (name, attached) = match l.split_once(':') {
            Some((n, _)) if n.starts_with('-') => {
                (n.to_owned(), Some(a.text[n.len() + 1..].to_owned()))
            }
            _ => (l.clone(), None),
        };
        if !name.starts_with('-') || name.len() < 2 {
            operands.push(a.clone());
            j += 1;
            continue;
        }
        let opt = &name[1..];
        let is = |full: &str, min: usize| opt.len() >= min && full.starts_with(opt);
        if is("path", 1) || is("literalpath", 1) {
            let joined = attached.is_some();
            match attached {
                Some(v) => operands.push(Word {
                    text: v,
                    dynamic: a.dynamic,
                }),
                None => operands.extend(args.get(j + 1).cloned()),
            }
            j += if joined { 1 } else { 2 };
        } else if is("recurse", 1) {
            recursive = true;
            j += 1;
        } else if is("force", 2) || is("confirm", 2) || is("whatif", 1) || is("verbose", 1) {
            j += 1;
        } else if is("include", 2)
            || is("exclude", 2)
            || is("filter", 2)
            || opt == "ea"
            || is("erroraction", 2)
        {
            if attached.is_none() {
                j += 1;
            }
            j += 1;
        } else {
            return false;
        }
    }
    if operands.is_empty() {
        // Remove-Item takes its targets from the pipeline (or prompts).
        let _ = piped;
        c.hold(InputNeeded::UnresolvedTarget);
        return false;
    }
    let kind = if recursive {
        DestructiveKind::RemoveTree
    } else {
        DestructiveKind::Unlink
    };
    c.push(kind, &operands)
}

/// Records the assessment of a dispatched command in history, attributed to the invoking
/// principal: a refusal or hold with its reason, or (for a command without any consequential
/// token under Full access) the residual risk it runs with. A recognized destructive step
/// records nothing here; its preflight decision does. The command text itself is not recorded;
/// only the program name, redacted, and a digest of the argv.
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
        .map(|p| Redactor::new().redact_text(&normalize(p)));
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

// ---------------------------------------------------------------------------------------------
// The launched child's environment

/// The environment a command will actually run with: what the child inherits (unless the launch
/// clears it) overlaid by the variables the launch sets. The preflight resolves `~` and
/// environment references against it, never against the host's own environment, because a
/// child with a different `HOME` (or any other variable) deletes different targets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildEnvironment {
    vars: Vec<(String, String)>,
    /// Variable names compare ignoring ASCII case (Windows).
    case_insensitive: bool,
}
impl ChildEnvironment {
    pub fn effective(
        inherited: &[(String, String)],
        clear: bool,
        set: &[(String, String)],
        case_insensitive: bool,
    ) -> Self {
        let mut env = Self {
            vars: Vec::new(),
            case_insensitive,
        };
        if !clear {
            for (k, v) in inherited {
                env.put(k, v);
            }
        }
        for (k, v) in set {
            env.put(k, v);
        }
        env
    }
    fn same(&self, a: &str, b: &str) -> bool {
        if self.case_insensitive {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }
    fn put(&mut self, k: &str, v: &str) {
        match self.vars.iter_mut().find(|(n, _)| {
            if self.case_insensitive {
                n.eq_ignore_ascii_case(k)
            } else {
                n == k
            }
        }) {
            Some(slot) => slot.1 = v.to_owned(),
            None => self.vars.push((k.to_owned(), v.to_owned())),
        }
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(n, _)| self.same(n, name))
            .map(|(_, v)| v.as_str())
    }
    /// The home directory `~` names for the child (`HOME`, else `USERPROFILE`).
    pub fn home(&self) -> Option<&str> {
        self.get("HOME")
            .or_else(|| self.get("USERPROFILE"))
            .filter(|h| !h.is_empty())
    }
    /// `text` with `$NAME`, `${NAME}` and `%NAME%` references replaced by the child's values.
    /// An undefined `$` reference becomes empty, as a POSIX shell expands it; an undefined `%`
    /// reference stays literal, as cmd leaves it. Quoting is ignored, so a reference a shell
    /// would not expand is expanded too: the result only adds targets to check.
    pub fn substitute(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '$' if chars.get(i + 1) == Some(&'{') => {
                    match chars[i + 2..].iter().position(|&c| c == '}') {
                        Some(n)
                            if chars[i + 2..i + 2 + n].iter().all(|&c| name_char(c)) && n > 0 =>
                        {
                            let name: String = chars[i + 2..i + 2 + n].iter().collect();
                            out.push_str(self.get(&name).unwrap_or(""));
                            i += n + 3;
                        }
                        _ => {
                            out.push('$');
                            i += 1;
                        }
                    }
                }
                '$' if chars
                    .get(i + 1)
                    .is_some_and(|&c| c.is_ascii_alphabetic() || c == '_') =>
                {
                    let n = chars[i + 1..]
                        .iter()
                        .position(|&c| !name_char(c))
                        .unwrap_or(chars.len() - i - 1);
                    let name: String = chars[i + 1..i + 1 + n].iter().collect();
                    out.push_str(self.get(&name).unwrap_or(""));
                    i += n + 1;
                }
                '%' => match chars[i + 1..].iter().position(|&c| c == '%') {
                    Some(n) if n > 0 && chars[i + 1..i + 1 + n].iter().all(|&c| name_char(c)) => {
                        let name: String = chars[i + 1..i + 1 + n].iter().collect();
                        match self.get(&name) {
                            Some(v) => out.push_str(v),
                            None => out.extend(&chars[i..i + n + 2]),
                        }
                        i += n + 2;
                    }
                    _ => {
                        out.push('%');
                        i += 1;
                    }
                },
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }
}

/// Resolves `~` against the child's home instead of the host's. The protected home stays the
/// host user's own ([`FilesystemResolver::home`] is passed through).
struct ChildResolver<'a> {
    inner: &'a dyn FilesystemResolver,
    home: Option<String>,
}
impl ChildResolver<'_> {
    fn rewrite(&self, path: &str) -> Result<String, ResolveError> {
        let rest = match path {
            "~" => Some(""),
            _ => path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")),
        };
        match (rest, &self.home) {
            (None, _) => Ok(path.to_owned()),
            (Some(rest), Some(home)) => Ok(if rest.is_empty() {
                home.clone()
            } else {
                format!("{}/{rest}", home.trim_end_matches(['/', '\\']))
            }),
            (Some(_), None) => Err(ResolveError::Invalid(
                "the launched command has no home directory".into(),
            )),
        }
    }
    fn cwd(&self, working_directory: Option<&str>) -> Result<Option<String>, ResolveError> {
        working_directory.map(|w| self.rewrite(w)).transpose()
    }
}
impl FilesystemResolver for ChildResolver<'_> {
    fn resolve(
        &self,
        path: &str,
        working_directory: Option<&str>,
    ) -> Result<ResolvedPath, ResolveError> {
        let cwd = self.cwd(working_directory)?;
        self.inner.resolve(&self.rewrite(path)?, cwd.as_deref())
    }
    fn expand(
        &self,
        pattern: &str,
        working_directory: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, ResolveError> {
        let cwd = self.cwd(working_directory)?;
        self.inner
            .expand(&self.rewrite(pattern)?, cwd.as_deref(), limit)
    }
    fn home(&self) -> Option<std::path::PathBuf> {
        self.inner.home()
    }
    fn gitfile_target(&self, gitfile: &std::path::Path) -> Option<String> {
        self.inner.gitfile_target(gitfile)
    }
}

/// [`preflight_command`] for a command that will run with `env`: `~` resolves to the child's
/// home, and the argv with its environment references substituted by the child's values is
/// preflighted as well. A refusal of either analysis refuses the command (approval never
/// unlocks it); otherwise the verdict of the command as written applies, so a reference the
/// structured analysis cannot model stays held.
pub fn preflight_command_in(
    fs: &dyn FilesystemResolver,
    roots: &ProtectedRoots,
    argv: &[String],
    working_directory: Option<&str>,
    protected_branches: &[String],
    env: &ChildEnvironment,
) -> CommandVerdict {
    let child = ChildResolver {
        inner: fs,
        home: env.home().map(str::to_owned),
    };
    let written = preflight_command(&child, roots, argv, working_directory, protected_branches);
    if matches!(written, CommandVerdict::Blocked { .. }) {
        return written;
    }
    let substituted: Vec<String> = argv.iter().map(|a| env.substitute(a)).collect();
    let cwd = working_directory.map(|w| env.substitute(w));
    if substituted.as_slice() != argv || cwd.as_deref() != working_directory {
        let expanded = preflight_command(
            &child,
            roots,
            &substituted,
            cwd.as_deref(),
            protected_branches,
        );
        if matches!(expanded, CommandVerdict::Blocked { .. }) {
            return expanded;
        }
    }
    written
}
