//! Shell reserved words count in command position. Literals and values keep their word boundary.
use super::{
    complexity::{self, Event, Line},
    scores::Complexity,
};
use regex::Regex;
use std::sync::LazyLock;

enum Frame {
    Single,
    Ansi,
    Double,
    Command(usize),
    Parameter(usize),
    Arithmetic(usize),
    Heredoc { name: String, tabs: bool },
}

static HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^<<(-?)[ \t]*(?:'([^']*)'|"([^"]*)"|\\?([^\s;&|()<>'"]+))"#)
        .expect("shell heredoc syntax")
});
const OP: &[u8] = b";&|()<>";
const WIDE: &[&str] = &[
    ";;", ";&", "&&", "||", "|&", ">>", "<&", ">&", "&>", "<>", ">|",
];

#[derive(Default)]
struct Lexer {
    frames: Vec<Frame>,
    continued: bool,
}

impl Lexer {
    fn line(&mut self, line: &str) -> String {
        let line = line.trim_end_matches('\r');
        self.continued = false;
        if let Some(Frame::Heredoc { name, tabs }) = self.frames.last() {
            let line = if *tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if line == name {
                self.frames.pop();
            }
            return String::new();
        }
        let bytes = line.as_bytes();
        let mut position = 0;
        let mut code = Vec::with_capacity(line.len());
        let mut heredocs = Vec::new();
        while position < bytes.len() {
            let rest = &bytes[position..];
            let ch = rest[0];
            match self.frames.last_mut() {
                Some(Frame::Single | Frame::Ansi) => {
                    if ch == b'\\' && matches!(self.frames.last(), Some(Frame::Ansi)) {
                        position += 2;
                        continue;
                    }
                    if ch == b'\'' {
                        self.frames.pop();
                    }
                }
                Some(Frame::Double) => {
                    if ch == b'\\' {
                        position += 2;
                        continue;
                    }
                    if ch == b'"' {
                        self.frames.pop();
                    } else if let Some(take) = self.expansion(rest, &mut code) {
                        position += take;
                        continue;
                    }
                }
                Some(Frame::Parameter(depth)) => match ch {
                    b'\\' => {
                        position += 2;
                        continue;
                    }
                    b'{' => *depth += 1,
                    b'}' => {
                        if *depth == 0 {
                            self.frames.pop();
                        } else {
                            *depth -= 1;
                        }
                    }
                    b'\'' => self.frames.push(Frame::Single),
                    b'"' => self.frames.push(Frame::Double),
                    _ => {}
                },
                Some(Frame::Arithmetic(depth)) => {
                    if ch == b'(' {
                        *depth += 1;
                    } else if ch == b')' {
                        *depth -= 1;
                        if *depth == 0 {
                            self.frames.pop();
                            position += 2;
                            continue;
                        }
                    }
                }
                Some(Frame::Heredoc { .. }) => break,
                _ => {
                    if let Some(Frame::Command(depth)) = self.frames.last_mut() {
                        if ch == b'(' {
                            *depth += 1;
                        } else if ch == b')' {
                            if *depth == 0 {
                                self.frames.pop();
                                code.extend_from_slice(b" ) ");
                                position += 1;
                                continue;
                            }
                            *depth -= 1;
                        }
                    }
                    if ch == b'\\' {
                        if rest.len() == 1 {
                            self.continued = true;
                            break;
                        }
                        code.push(b'0');
                        position += 2;
                        continue;
                    }
                    if ch == b'#'
                        && (position == 0
                            || bytes[position - 1].is_ascii_whitespace()
                            || OP.contains(&bytes[position - 1]))
                    {
                        break;
                    }
                    if let Some((frame, take)) = shell_literal(rest) {
                        self.frames.push(frame);
                        code.push(b'0');
                        position += take;
                        continue;
                    }
                    if let Some(take) = self.expansion(rest, &mut code) {
                        position += take;
                        continue;
                    }
                    if rest.starts_with(b"<<<") {
                        code.extend_from_slice(b"<<<");
                        position += 3;
                        continue;
                    }
                    if rest.starts_with(b"<<")
                        && let Some(suffix) = line.get(position..)
                        && let Some(parts) = HEREDOC.captures(suffix)
                    {
                        let name = parts
                            .get(2)
                            .or_else(|| parts.get(3))
                            .or_else(|| parts.get(4))
                            .expect("heredoc delimiter");
                        heredocs.push(Frame::Heredoc {
                            name: name.as_str().into(),
                            tabs: &parts[1] == "-",
                        });
                        position += parts.get(0).expect("heredoc").len();
                        code.push(b' ');
                        continue;
                    }
                    code.push(ch);
                }
            }
            position += 1;
        }
        self.frames.extend(heredocs.into_iter().rev());
        String::from_utf8_lossy(&code).into_owned()
    }

    fn expansion(&mut self, rest: &[u8], code: &mut Vec<u8>) -> Option<usize> {
        let (frame, take, marker): (_, _, &[u8]) = if rest.starts_with(b"$((") {
            (Frame::Arithmetic(1), 3, b"0")
        } else if rest.starts_with(b"$(") {
            (Frame::Command(0), 2, b" ( ")
        } else if rest.starts_with(b"${") {
            (Frame::Parameter(0), 2, b"0")
        } else {
            return None;
        };
        self.frames.push(frame);
        code.extend_from_slice(marker);
        Some(take)
    }
}

fn shell_literal(rest: &[u8]) -> Option<(Frame, usize)> {
    if rest.starts_with(b"$'") {
        Some((Frame::Ansi, 2))
    } else if rest.starts_with(b"((") {
        Some((Frame::Arithmetic(1), 2))
    } else {
        match rest[0] {
            b'\'' => Some((Frame::Single, 1)),
            b'"' => Some((Frame::Double, 1)),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Parser {
    lexer: Lexer,
    cases: Vec<bool>,
    command: bool,
    case_awaits_in: bool,
    function_awaits_name: bool,
}

pub(super) fn compute(source: &str) -> Complexity {
    let mut parser = Parser::default();
    complexity::blocks(source, |line, _| parser.parse(line))
}

impl Parser {
    fn parse(&mut self, line: &str) -> Option<Line> {
        let continued = self.lexer.continued;
        let code = self.lexer.line(line);
        if !continued {
            self.command = true;
        }
        let tokens = tokenize(&code);
        if tokens.is_empty() {
            return None;
        }
        let mut result = Line::default();
        let mut at = 0;
        while at < tokens.len() {
            let token = tokens[at];
            at += 1;
            let pattern = self.cases.last() == Some(&true);
            if OP.contains(&token.as_bytes()[0]) {
                match token {
                    ";;" | ";&" | ";;&" => {
                        if let Some(case) = self.cases.last_mut() {
                            *case = true;
                        }
                        self.command = true;
                    }
                    ")" if pattern => {
                        result.branches += 1;
                        if let Some(case) = self.cases.last_mut() {
                            *case = false;
                        }
                        self.command = true;
                    }
                    "(" | "|" if pattern => {}
                    "<" | ">" | ">>" | "<&" | ">&" | "&>" | "<>" | ">|" => {}
                    _ => {
                        result.branches += usize::from(matches!(token, "&&" | "||"));
                        self.command = true;
                    }
                }
                continue;
            }
            if pattern {
                if token == "esac" {
                    self.cases.pop();
                    result.events.push(Event::Close);
                    self.command = false;
                }
                continue;
            }
            if self.case_awaits_in {
                if token == "in" {
                    self.case_awaits_in = false;
                    self.cases.push(true);
                }
                continue;
            }
            if self.function_awaits_name {
                self.function_awaits_name = false;
                self.command = true;
                continue;
            }
            if !self.command {
                continue;
            }
            match token {
                "if" | "while" | "until" | "for" | "select" | "case" => {
                    result.events.push(Event::Open {
                        function_start: None,
                    });
                    result.branches += 1;
                    self.command = !matches!(token, "for" | "select" | "case");
                    self.case_awaits_in = token == "case";
                }
                "elif" => result.branches += 1,
                "then" | "else" | "do" | "!" | "time" => {}
                "esac" | "fi" | "done" | "}" => {
                    if token == "esac" {
                        self.cases.pop();
                    }
                    result.events.push(Event::Close);
                    self.command = false;
                }
                "{" => result.events.push(Event::Open {
                    function_start: None,
                }),
                "function" => {
                    result.declaration = true;
                    self.function_awaits_name = true;
                }
                _ if tokens.get(at..at + 2) == Some(&["(", ")"]) => {
                    result.declaration = true;
                    at += 2;
                }
                _ => self.command = false,
            }
        }
        Some(result)
    }
}

fn tokenize(mut code: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    while !code.trim_start().is_empty() {
        code = code.trim_start();
        let take = if OP.contains(&code.as_bytes()[0]) {
            if code.starts_with(";;&") {
                3
            } else {
                WIDE.iter()
                    .find(|op| code.starts_with(**op))
                    .map_or(1, |op| op.len())
            }
        } else {
            code.char_indices()
                .find(|(_, c)| c.is_whitespace() || ";&|()<>".contains(*c))
                .map_or(code.len(), |(position, _)| position)
        };
        tokens.push(&code[..take]);
        code = &code[take..];
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heredoc_and_printed_keywords_are_ignored_but_substitution_is_code() {
        let text = "f() {\n    cat <<-EOF\n\t\tif x; then\n\tEOF\n    echo if\n    echo \"$(if yes; then value; fi)\"\n    if ok; then\n      echo a\n    fi\n}";
        let score = compute(text);
        assert_eq!(
            (
                score.functions,
                score.longest_function,
                score.branches,
                score.nesting
            ),
            (1, 10, 2, 2)
        );
    }
}
