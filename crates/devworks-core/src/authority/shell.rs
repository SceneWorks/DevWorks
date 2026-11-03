//! Shell/process and declared-effect preflight input (SEC-05).
//!
//! Turns an argv (including `sh -c`, `cmd /c` and PowerShell `-Command` scripts) or a declared
//! MCP/package effect into [`EffectPlan`]s for [`super::destructive::preflight`]. It recognizes
//! common deletion commands, their recursion flags, `find -delete`/`-exec`, `xargs` and `>`
//! redirections, and refuses destructive steps whose arguments depend on runtime expansion.
//! Recognition is deliberately incomplete: arbitrary scripts, interpreters and native programs
//! cannot be proven safe, so an unrecognized command is reported with its residual risk, never
//! as harmless.
use super::destructive::{DestructiveKind, EffectPlan, InputNeeded, IntendedScope};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Posix,
    Cmd,
    PowerShell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word { text: String, expands: bool },
    Separator,
    Redirect { truncate: bool },
}

/// Splits a script into words, separators and redirections. Single quotes are literal; any
/// unquoted or double-quoted `$`, backtick or `%NAME%` marks the word as runtime-expanded.
fn tokenize(script: &str, dialect: Dialect) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut expands = false;
    let mut chars = script.chars().peekable();
    let flush =
        |out: &mut Vec<Token>, word: &mut String, in_word: &mut bool, expands: &mut bool| {
            if *in_word {
                out.push(Token::Word {
                    text: std::mem::take(word),
                    expands: *expands,
                });
            }
            *in_word = false;
            *expands = false;
        };
    while let Some(c) = chars.next() {
        match c {
            '\'' if dialect != Dialect::Cmd => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if dialect == Dialect::Posix => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        '$' | '`' => {
                            expands = true;
                            word.push(q);
                        }
                        '%' if dialect == Dialect::Cmd => {
                            expands = true;
                            word.push(q);
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' if dialect == Dialect::Posix => {
                in_word = true;
                if let Some(n) = chars.next() {
                    word.push(n);
                }
            }
            ';' | '\n' | '|' | '&' => {
                flush(&mut out, &mut word, &mut in_word, &mut expands);
                if c == '&' && chars.peek() == Some(&'>') {
                    chars.next();
                    out.push(Token::Redirect { truncate: true });
                } else {
                    while chars.peek().is_some_and(|n| *n == c) {
                        chars.next();
                    }
                    out.push(Token::Separator);
                }
            }
            '>' => {
                // `2>` style descriptor prefixes are part of the redirection.
                if word.chars().all(|d| d.is_ascii_digit()) && in_word {
                    word.clear();
                    in_word = false;
                    expands = false;
                } else {
                    flush(&mut out, &mut word, &mut in_word, &mut expands);
                }
                let append = chars.peek() == Some(&'>');
                if append {
                    chars.next();
                }
                out.push(Token::Redirect { truncate: !append });
            }
            c if c.is_whitespace() => flush(&mut out, &mut word, &mut in_word, &mut expands),
            '$' | '`' => {
                in_word = true;
                expands = true;
                word.push(c);
            }
            '%' if dialect == Dialect::Cmd => {
                in_word = true;
                expands = true;
                word.push(c);
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    flush(&mut out, &mut word, &mut in_word, &mut expands);
    out
}

fn program(word: &str) -> String {
    let base = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_owned()
}

struct Collector {
    plans: Vec<EffectPlan>,
    needs: Option<InputNeeded>,
}

/// Assesses one argv as dispatched (no implicit shell).
pub fn assess_command(argv: &[String], working_directory: Option<&str>) -> ShellAssessment {
    let words: Vec<(String, bool)> = argv.iter().map(|a| (a.clone(), false)).collect();
    let mut c = Collector {
        plans: Vec::new(),
        needs: None,
    };
    command(&words, Dialect::Posix, working_directory, &mut c, 0);
    finish(c)
}

fn finish(c: Collector) -> ShellAssessment {
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

fn script(text: &str, dialect: Dialect, cwd: Option<&str>, c: &mut Collector, depth: u8) {
    let tokens = tokenize(text, dialect);
    let mut current: Vec<(String, bool)> = Vec::new();
    let mut redirect = None;
    let run = |current: &mut Vec<(String, bool)>, c: &mut Collector| {
        command(current, dialect, cwd, c, depth + 1);
        current.clear();
    };
    for t in tokens {
        match t {
            Token::Separator => run(&mut current, c),
            Token::Redirect { truncate } => redirect = Some(truncate),
            Token::Word { text, expands } => match redirect.take() {
                Some(truncate) => {
                    if text.eq_ignore_ascii_case("/dev/null") || text.eq_ignore_ascii_case("nul") {
                        continue;
                    }
                    if expands {
                        c.needs = Some(InputNeeded::UnresolvedExpansion);
                    } else if truncate {
                        c.plans
                            .push(plan(DestructiveKind::Truncate, vec![text], cwd));
                    }
                }
                None => current.push((text, expands)),
            },
        }
    }
    run(&mut current, c);
}

/// Targets of a destructive command; any runtime expansion refuses the step.
fn targets(args: &[(String, bool)], c: &mut Collector) -> Option<Vec<String>> {
    if args.iter().any(|(_, e)| *e) {
        c.needs = Some(InputNeeded::UnresolvedExpansion);
        return None;
    }
    Some(args.iter().map(|(a, _)| a.clone()).collect())
}

fn command(
    words: &[(String, bool)],
    dialect: Dialect,
    cwd: Option<&str>,
    c: &mut Collector,
    depth: u8,
) {
    if words.is_empty() || depth > 8 {
        return;
    }
    let mut i = 0;
    // Wrappers that run the rest of the line as a command.
    loop {
        let Some((w, _)) = words.get(i) else { return };
        let name = program(w);
        match name.as_str() {
            "env" | "sudo" | "doas" | "nice" | "nohup" | "command" | "exec" | "time"
            | "builtin" => {
                i += 1;
                while words
                    .get(i)
                    .is_some_and(|(a, _)| a.starts_with('-') || (name == "env" && a.contains('=')))
                {
                    i += 1;
                }
            }
            _ if w.contains('=') && !w.starts_with('-') && dialect == Dialect::Posix => i += 1,
            _ => break,
        }
    }
    let (head, _) = &words[i];
    let name = program(head);
    let args = &words[i + 1..];
    match name.as_str() {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => {
            let mut j = 0;
            while let Some((a, _)) = args.get(j) {
                if a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
                    if let Some((body, _)) = args.get(j + 1) {
                        script(body, Dialect::Posix, cwd, c, depth);
                    }
                    return;
                }
                if !a.starts_with('-') {
                    // A script file: opaque.
                    return;
                }
                j += 1;
            }
        }
        "cmd" => {
            if let Some(k) = args
                .iter()
                .position(|(a, _)| a.eq_ignore_ascii_case("/c") || a.eq_ignore_ascii_case("/k"))
            {
                let body: Vec<&str> = args[k + 1..].iter().map(|(a, _)| a.as_str()).collect();
                script(&body.join(" "), Dialect::Cmd, cwd, c, depth);
            }
        }
        "powershell" | "pwsh" => {
            if let Some(k) = args.iter().position(|(a, _)| {
                let a = a.to_lowercase();
                a == "-command" || a == "-c"
            }) {
                let body: Vec<&str> = args[k + 1..].iter().map(|(a, _)| a.as_str()).collect();
                script(&body.join(" "), Dialect::PowerShell, cwd, c, depth);
            }
        }
        "xargs" => {
            // Targets arrive on standard input.
            if let Some(k) = args.iter().position(|(a, _)| !a.starts_with('-')) {
                let inner = program(&args[k].0);
                if is_deleter(&inner) {
                    c.needs = Some(InputNeeded::UnresolvedTarget);
                }
            }
        }
        "find" => {
            let paths: Vec<(String, bool)> = args
                .iter()
                .take_while(|(a, _)| !a.starts_with('-') && a != "(" && a != "!")
                .cloned()
                .collect();
            let deletes = args.iter().any(|(a, _)| a == "-delete")
                || args.windows(2).any(|w| {
                    matches!(w[0].0.as_str(), "-exec" | "-execdir" | "-ok")
                        && is_deleter(&program(&w[1].0))
                });
            if deletes {
                let paths = if paths.is_empty() {
                    vec![(".".to_owned(), false)]
                } else {
                    paths
                };
                if let Some(t) = targets(&paths, c) {
                    c.plans.push(plan(DestructiveKind::EraseContents, t, cwd));
                }
            }
        }
        "rm" | "remove-item" | "ri" | "del" | "erase" | "rd" | "rmdir" | "unlink" | "shred" => {
            removal(&name, args, dialect, cwd, c)
        }
        _ => {}
    }
}

fn is_deleter(name: &str) -> bool {
    matches!(
        name,
        "rm" | "rmdir" | "unlink" | "shred" | "del" | "erase" | "rd" | "remove-item" | "ri"
    )
}

fn removal(
    name: &str,
    args: &[(String, bool)],
    dialect: Dialect,
    cwd: Option<&str>,
    c: &mut Collector,
) {
    let cmd_style = dialect == Dialect::Cmd || matches!(name, "del" | "erase" | "rd");
    let powershell = dialect == Dialect::PowerShell || matches!(name, "remove-item" | "ri");
    let mut recursive = false;
    let mut operands = Vec::new();
    let mut end_of_flags = false;
    for (a, e) in args {
        let lower = a.to_lowercase();
        if !end_of_flags && a == "--" {
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
                recursive |= a[1..].contains(['r', 'R']);
            }
        } else {
            operands.push((a.clone(), *e));
        }
    }
    if operands.is_empty() {
        return;
    }
    let Some(t) = targets(&operands, c) else {
        return;
    };
    let kind = match name {
        "unlink" => DestructiveKind::Unlink,
        "shred" => DestructiveKind::Truncate,
        "del" | "erase" if recursive => DestructiveKind::EraseContents,
        _ if recursive => DestructiveKind::RemoveTree,
        _ => DestructiveKind::Unlink,
    };
    c.plans.push(plan(kind, t, cwd));
}
