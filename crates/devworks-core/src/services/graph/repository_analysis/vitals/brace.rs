//! Retained block events and declaration recognition for Swift, Rust, C/C++, Go and PHP.
use super::{
    brace_lexer::{Dialect, Lexer},
    complexity::{self, Event, Line},
    scores::Complexity,
};
use regex::Regex;
use std::sync::LazyLock;

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("retained complexity pattern")
}

static SWIFT_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(^|[\s(])(func\s+[^\s(<]+|init[?!]?\s*[(<]|deinit\b)"));
static SWIFT_BRANCH: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"\b(if|guard|switch|case|for|while|repeat|catch)\b|&&|\|\||\?\?|\s\?\s")
});
static RUST_FUNCTION: LazyLock<Regex> = LazyLock::new(|| regex(r"\bfn\s+[A-Za-z_]"));
static RUST_BRANCH: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\b(if|match|for|while|loop)\b|=>|&&|\|\|"));
static C_BRANCH: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\b(if|for|while|switch|case|catch)\b|&&|\|\||\s\?\s"));
static GO_FUNCTION: LazyLock<Regex> = LazyLock::new(|| regex(r"^func\s*(\([^)]*\)\s*)?[A-Za-z_]"));
static GO_BRANCH: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\b(if|for|switch|select|case)\b|&&|\|\|"));
static PHP_FUNCTION: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\bfunction\s+&?\s*[\pL_]"));
static PHP_BRANCH: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\b(if|elseif|for|foreach|while|switch|case|catch|match|and|or)\b|&&|\|\||\?\?|\s\?\s",
    )
});
static TRAILING_RESULT: LazyLock<Regex> = LazyLock::new(|| regex(r"\s*->[^()]*$"));
static QUALIFIER: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"\s*(\b(const|volatile|noexcept|override|final|mutable|try)\b|&&|&)\s*$")
});
static NAME: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(~?[A-Za-z_]\w*|\boperator\s*[^\s\w(]+)\s*$"));
static K_AND_R: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(.*?\b[A-Za-z_]\w*\s*\(\s*[A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*\s*\))\s+[A-Za-z_][\w\s*,\[\]]*$",
    )
});
static K_AND_R_PARAMETER: LazyLock<Regex> = LazyLock::new(|| regex(r"^[A-Za-z_][\w\s*,\[\]]*$"));
static INITIALIZER: LazyLock<Regex> = LazyLock::new(|| regex(r"\)\s*(?:noexcept\s*)?:"));
static ENDS_WITH_NAME: LazyLock<Regex> = LazyLock::new(|| regex(r"[A-Za-z_][\w:<>]*\s*$"));
static ACCESS: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(public|private|protected)(\s+(slots|Q_SLOTS))?$|^(signals|Q_SIGNALS)$")
});

const CONTROL: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "sizeof",
    "else",
    "do",
    "case",
    "new",
    "delete",
    "throw",
    "alignof",
    "decltype",
    "typeid",
    "static_assert",
    "co_return",
    "co_await",
    "co_yield",
];

pub(super) fn compute(extension: &str, source: &str) -> Option<Complexity> {
    let dialect = match extension {
        "swift" => Dialect::Swift,
        "rs" => Dialect::Rust,
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Dialect::C,
        "go" => Dialect::Go,
        "php" => Dialect::Php,
        _ => return None,
    };
    let mut lexer = Lexer::new(dialect);
    let mut declaration = CHead::default();
    Some(complexity::blocks(source, |line, index| {
        let stripped = lexer.strip(line);
        let code = stripped.trim();
        if code.is_empty() {
            return None;
        }
        let mut events = Vec::new();
        if dialect == Dialect::C {
            declaration.read(code, index, &mut events);
        } else {
            for byte in code.bytes() {
                match byte {
                    b'{' => events.push(Event::Open {
                        function_start: None,
                    }),
                    b'}' => events.push(Event::Close),
                    _ => {}
                }
            }
        }
        let (declaration, branches) = match dialect {
            Dialect::Swift => (
                SWIFT_FUNCTION.is_match(code),
                SWIFT_BRANCH.find_iter(code).count(),
            ),
            Dialect::Rust => (
                RUST_FUNCTION.is_match(code),
                RUST_BRANCH.find_iter(code).count() + rust_questions(code),
            ),
            Dialect::C => (false, C_BRANCH.find_iter(code).count()),
            Dialect::Go => (
                GO_FUNCTION.is_match(code),
                GO_BRANCH.find_iter(code).count(),
            ),
            Dialect::Php => (php_function(code), php_branches(code)),
        };
        Some(Line {
            events,
            declaration,
            branches,
            ends_statement: code.ends_with(';'),
        })
    }))
}

fn rust_questions(code: &str) -> usize {
    code.match_indices('?')
        .filter(|(at, _)| {
            code[at + 1..].chars().next().is_none_or(|next| {
                next.is_whitespace() || matches!(next, ';' | '.' | ')' | '?' | ',')
            })
        })
        .count()
}
fn php_function(code: &str) -> bool {
    PHP_FUNCTION.find_iter(code).any(|matched| {
        !code[..matched.start()]
            .split_whitespace()
            .next_back()
            .is_some_and(|word| word.eq_ignore_ascii_case("use"))
    })
}
fn php_branches(code: &str) -> usize {
    PHP_BRANCH
        .find_iter(code)
        .filter(|matched| {
            let prefix = &code[..matched.start()];
            !["->", "::", "$"]
                .iter()
                .any(|operator| prefix.ends_with(operator))
        })
        .count()
}

#[derive(Default)]
struct CHead {
    text: String,
    start: usize,
    knr: Option<usize>,
    initializer_depth: usize,
}

impl CHead {
    fn read(&mut self, code: &str, index: usize, events: &mut Vec<Event>) {
        for character in code.chars() {
            if self.initializer_depth > 0 {
                if character == '{' {
                    self.initializer_depth += 1;
                } else if character == '}' {
                    self.initializer_depth -= 1;
                    if self.initializer_depth == 0 {
                        self.text.push_str("()");
                    }
                }
                continue;
            }
            match character {
                '{' if member_initializer(&self.text) => self.initializer_depth = 1,
                '{' => {
                    let function_start = if function_declarator(&self.text) {
                        Some(self.start)
                    } else if self.text.trim().is_empty() {
                        self.knr
                    } else {
                        None
                    };
                    events.push(Event::Open { function_start });
                    self.text.clear();
                    self.knr = None;
                }
                '}' => {
                    events.push(Event::Close);
                    self.text.clear();
                    self.knr = None;
                }
                ';' => {
                    let statement = self.text.trim();
                    if self.knr.is_some() {
                        if !K_AND_R_PARAMETER.is_match(statement) {
                            self.knr = None;
                        }
                    } else if let Some(parts) = K_AND_R.captures(statement)
                        && function_declarator(&parts[1])
                    {
                        self.knr = Some(self.start);
                    }
                    self.text.clear();
                }
                ':' if self.text.len() <= 32 && ACCESS.is_match(self.text.trim()) => {
                    self.text.clear()
                }
                character => {
                    if self.text.is_empty() {
                        if character.is_whitespace() {
                            continue;
                        }
                        self.start = index;
                    }
                    self.text.push(character);
                }
            }
        }
        if !self.text.is_empty() {
            self.text.push(' ');
        }
    }
}

fn member_initializer(head: &str) -> bool {
    INITIALIZER
        .find_iter(head)
        .find(|token| !head[token.end()..].starts_with(':'))
        .is_some_and(|token| {
            ENDS_WITH_NAME.is_match(&head[token.end()..])
                && function_declarator(&head[..token.start() + 1])
        })
}
fn function_declarator(head: &str) -> bool {
    let result = TRAILING_RESULT.replace(head.trim(), "");
    let mut head = result.as_ref();
    while let Some(qualifier) = QUALIFIER.find(head) {
        head = &head[..qualifier.start()];
    }
    if !head.ends_with(')') {
        return false;
    }
    head.find('(')
        .and_then(|open| NAME.captures(&head[..open]))
        .is_some_and(|name| !CONTROL.contains(&&name[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_declarators_preserve_knr_and_skip_dead_preprocessor_branches() {
        let source = "#if 0\nint dead() { if (x) { if (y) { bad(); } } }\n#else\nint add(a, b)\nint a;\nint b;\n{\n  if (a) { return a + b; }\n  return b;\n}\n#endif";
        let result = compute("c", source).unwrap();
        assert_eq!(
            (
                result.functions,
                result.nesting,
                result.branches,
                result.longest_function
            ),
            (1, 2, 1, 7)
        );
    }

    #[test]
    fn rust_raw_literals_nested_comments_and_lifetimes_do_not_add_braces() {
        let source = r##"fn parse<'a>(x: &'a str) {
    let text = r#"{ if x { } }"#;
    /* { /* { } */ } */
    if x.is_empty() {
        parse(x)?;
    }
}"##;
        let result = compute("rs", source).unwrap();
        assert_eq!(
            (
                result.functions,
                result.nesting,
                result.branches,
                result.longest_function
            ),
            (1, 2, 2, 7)
        );
    }

    #[test]
    fn php_only_measures_code_between_tags_and_tracks_nonkeyword_members() {
        let source = "unparsed { if fake() }\n<?php\nfunction active($obj) {\n  $obj->match();\n  if ($obj->works) { return 1; }\n}\n?> ignored {";
        let result = compute("php", source).unwrap();
        assert_eq!(
            (
                result.functions,
                result.nesting,
                result.branches,
                result.longest_function
            ),
            (1, 2, 1, 4)
        );
    }
}
