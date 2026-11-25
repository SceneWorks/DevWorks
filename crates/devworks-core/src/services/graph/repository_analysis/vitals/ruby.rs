//! Ruby blocks, statement modifiers and literal masking for retained file metrics.
use super::{
    complexity::{self, Event, Line},
    scores::Complexity,
};
use regex::Regex;
use std::sync::LazyLock;

const VALUE_KEYWORDS: &[&str] = &[
    "if", "unless", "elsif", "when", "while", "until", "and", "or", "not", "return", "then",
    "else", "do", "case", "in", "yield", "begin",
];
const STATEMENT_WORDS: &[&str] = &[
    "then", "else", "do", "begin", "and", "or", "not", "ensure", "elsif", "when", "in",
];
const BRANCHES: &[&str] = &[
    "if", "unless", "elsif", "while", "until", "for", "case", "when", "rescue", "and", "or",
];
const OPERATORS: &[u8] = b"+-*/%<>=!~^&|[]\x60@";

static HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^<<([~-]?)(?:'([^']+)'|"([^"]+)"|\x60([^\x60]+)\x60|([A-Za-z_]\w*))"#)
        .expect("ruby heredoc syntax")
});
static DEF_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(?:[A-Za-z_]\w*\.)?(?:[A-Za-z_]\w*[?!]?=?|\[\]=?|\*\*|<=>|===?|=~|!=|!~|<<|>>|<=|>=|[+-]@|[+\-*/%\x60~!<>&|^])")
        .expect("ruby method name")
});
static ENDLESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\s*\([^)]*\))?\s*=").expect("ruby endless definition"));

enum Frame {
    Literal {
        open: u8,
        close: u8,
        depth: usize,
        interpolate: bool,
        regex: bool,
    },
    Expression(usize),
    Heredoc {
        name: String,
        indented: bool,
        interpolate: bool,
    },
}

impl Frame {
    fn simple(close: u8, interpolate: bool, regex: bool) -> Self {
        Self::Literal {
            open: 0,
            close,
            depth: 0,
            interpolate,
            regex,
        }
    }
}

#[derive(Default)]
struct Lexer {
    stack: Vec<Frame>,
    block_comment: bool,
    data_section: bool,
}

impl Lexer {
    fn strip(&mut self, line: &str) -> String {
        let line = line.trim_end_matches('\r');
        if self.data_section {
            return String::new();
        }
        if self.stack.is_empty() {
            if self.block_comment {
                self.block_comment = !marker(line, "=end");
                return String::new();
            }
            if marker(line, "=begin") {
                self.block_comment = true;
                return String::new();
            }
            if line == "__END__" {
                self.data_section = true;
                return String::new();
            }
        }
        if let Some(Frame::Heredoc { name, indented, .. }) = self.stack.last() {
            let candidate = if *indented { line.trim() } else { line };
            if candidate == name {
                self.stack.pop();
                return String::new();
            }
        }
        let bytes = line.as_bytes();
        let mut code = Vec::with_capacity(line.len());
        let mut index = 0;
        let mut pending = Vec::new();
        while index < bytes.len() {
            let tail = &bytes[index..];
            let byte = tail[0];
            match self.stack.last_mut() {
                Some(Frame::Literal {
                    open,
                    close,
                    depth,
                    interpolate,
                    regex,
                }) => {
                    if byte == b'\\' {
                        index += 2;
                        continue;
                    }
                    if *interpolate && tail.starts_with(b"#{") {
                        self.stack.push(Frame::Expression(0));
                        code.push(b'(');
                        index += 2;
                        continue;
                    }
                    if byte == *open && *open != 0 {
                        *depth += 1;
                    } else if byte == *close {
                        if *depth > 0 {
                            *depth -= 1;
                        } else {
                            let regex = *regex;
                            self.stack.pop();
                            index += 1;
                            if regex {
                                index += bytes[index..]
                                    .iter()
                                    .take_while(|b| b.is_ascii_lowercase())
                                    .count();
                            }
                            continue;
                        }
                    }
                    index += 1;
                    continue;
                }
                Some(Frame::Heredoc { interpolate, .. }) => {
                    if byte == b'\\' {
                        index += 2;
                    } else if *interpolate && tail.starts_with(b"#{") {
                        self.stack.push(Frame::Expression(0));
                        code.push(b'(');
                        index += 2;
                    } else {
                        index += 1;
                    }
                    continue;
                }
                Some(Frame::Expression(depth)) => {
                    if byte == b'{' {
                        *depth += 1;
                    } else if byte == b'}' {
                        if *depth == 0 {
                            self.stack.pop();
                            code.push(b')');
                            index += 1;
                            continue;
                        }
                        *depth -= 1;
                    }
                }
                None => {}
            }
            if let Some(skip) = self.open(line, index, &mut code, &mut pending) {
                code.push(b'0');
                index += skip;
            } else if byte == b'#' {
                break;
            } else {
                code.push(byte);
                index += 1;
            }
        }
        self.stack.extend(pending.into_iter().rev());
        String::from_utf8_lossy(&code).into_owned()
    }

    fn open(
        &mut self,
        line: &str,
        at: usize,
        code: &mut Vec<u8>,
        pending: &mut Vec<Frame>,
    ) -> Option<usize> {
        let bytes = line.as_bytes();
        let tail = &bytes[at..];
        let byte = tail[0];
        if !matches!(byte, b'"' | b'\'' | b'$' | b'/' | b'?' | b'%' | b'<' | 96)
            || (matches!(byte, b'/' | b'%' | b'<' | b'?' | 96)
                && at > 0
                && bytes[at - 1] == b':'
                && (at < 2 || bytes[at - 2] != b':'))
        {
            return None;
        }
        let (last, word, after_def) = significant(code);
        if after_def {
            return None;
        }
        let space = at > 0 && bytes[at - 1].is_ascii_whitespace();
        let next = tail.get(1).copied().unwrap_or_default();
        let value = last == 0
            || b"=([{,;!~|&?:<>+-*/%^".contains(&last)
            || word.is_some_and(|word| VALUE_KEYWORDS.contains(&word));
        let argument =
            value || (word.is_some() && space && !next.is_ascii_whitespace() && next != b'=');
        let (frame, take) = match byte {
            b'"' | 96 => (Frame::simple(byte, true, false), 1),
            b'\'' => (Frame::simple(byte, false, false), 1),
            b'$' if next != 0 && !identifier(next) && next != b'{' => {
                code.push(b'$');
                return Some(2);
            }
            b'/' if argument => (Frame::simple(b'/', true, true), 1),
            b'?' if (value || space)
                && next != 0
                && !next.is_ascii_whitespace()
                && (next == b'\\' || !tail.get(2).is_some_and(|&next| identifier(next))) =>
            {
                return Some(if next == b'\\' { 3 } else { 2 });
            }
            b'%' => percent(tail, space, last, argument)?,
            b'<' if next == b'<' => {
                let captured = HEREDOC.captures(line.get(at..)?)?;
                let name = (2..=5).find_map(|index| captured.get(index))?.as_str();
                let bare = captured[1].is_empty() && captured.get(5).is_some();
                if bare
                    && !(name
                        .as_bytes()
                        .first()
                        .is_some_and(|c| c.is_ascii_uppercase() || *c == b'_')
                        && name
                            .bytes()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
                        && (value || space))
                {
                    return None;
                }
                pending.push(Frame::Heredoc {
                    name: name.into(),
                    indented: !captured[1].is_empty(),
                    interpolate: captured.get(2).is_none(),
                });
                return Some(captured.get(0)?.len());
            }
            _ => return None,
        };
        self.stack.push(frame);
        Some(take)
    }
}

fn percent(tail: &[u8], space: bool, last: u8, argument: bool) -> Option<(Frame, usize)> {
    let kind = tail.get(1).filter(|c| b"qQwWiIrsx".contains(c)).copied();
    let index = usize::from(kind.is_some()) + 1;
    let delimiter = *tail.get(index)?;
    if identifier(delimiter) || delimiter.is_ascii_whitespace() || delimiter == b'=' {
        return None;
    }
    let valid = if kind.is_some() {
        space || last == 0 || !(identifier(last) || matches!(last, b')' | b']'))
    } else {
        b"([{<|!/^".contains(&delimiter) && argument
    };
    if !valid {
        return None;
    }
    let close = match delimiter {
        b'(' => b')',
        b'[' => b']',
        b'{' => b'}',
        b'<' => b'>',
        _ => delimiter,
    };
    Some((
        Frame::Literal {
            open: if close == delimiter { 0 } else { delimiter },
            close,
            depth: 0,
            interpolate: kind.is_none_or(|tag| b"QWIrx".contains(&tag)),
            regex: kind == Some(b'r'),
        },
        index + 1,
    ))
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

fn starts_word(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || !byte.is_ascii()
}

fn marker(line: &str, prefix: &str) -> bool {
    line.strip_prefix(prefix)
        .is_some_and(|rest| rest.chars().next().is_none_or(char::is_whitespace))
}

fn significant(code: &[u8]) -> (u8, Option<&str>, bool) {
    let end = end_of_word(code);
    let Some(last) = code[..end].last().copied() else {
        return (0, None, false);
    };
    let base = end - usize::from(matches!(last, b'?' | b'!'));
    let start = code[..base]
        .iter()
        .rposition(|&b| !identifier(b))
        .map_or(0, |at| at + 1);
    let word = code[start..end]
        .first()
        .filter(|&&b| starts_word(b))
        .and_then(|_| std::str::from_utf8(&code[start..end]).ok());
    let after_def = if word == Some("def") {
        end < code.len()
    } else if last == b'.' {
        let receiver = code[..end - 1]
            .iter()
            .rposition(|&b| !identifier(b))
            .map_or(0, |at| at + 1);
        let before = end_of_word(&code[..receiver]);
        receiver < end - 1
            && before < receiver
            && code[..before].ends_with(b"def")
            && (before == 3 || !identifier(code[before - 4]))
    } else {
        false
    };
    (last, word, after_def)
}

fn end_of_word(code: &[u8]) -> usize {
    code.iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |at| at + 1)
}

#[derive(Default)]
struct Parser {
    lexer: Lexer,
    continued: bool,
    loop_awaits_do: bool,
}

pub(super) fn compute(source: &str) -> Complexity {
    let mut parser = Parser::default();
    complexity::blocks(source, |line, index| parser.parse(line, index))
}

impl Parser {
    fn parse(&mut self, line: &str, index: usize) -> Option<Line> {
        let code = self.lexer.strip(line);
        if code.trim().is_empty() {
            return None;
        }
        let bytes = code.as_bytes();
        let mut result = Line::default();
        let mut previous = self.continued.then_some("_");
        let last = *code.trim_end().as_bytes().last()?;
        self.continued = last == b'\\';
        let mut loop_awaits_do = self.loop_awaits_do;
        let mut position = 0;
        while position < bytes.len() {
            let start = position;
            let byte = bytes[start];
            let rest = &bytes[start..];
            if byte.is_ascii_whitespace() {
                position += 1;
                continue;
            }
            let mut token = "_";
            if starts_word(byte) || (matches!(byte, b'@' | b'$') && rest.len() > 1) {
                position += 1 + rest[1..]
                    .iter()
                    .take_while(|&&byte| identifier(byte) || byte == b'@')
                    .count();
                let following = bytes.get(position).copied().unwrap_or_default();
                let second = bytes.get(position + 1).copied().unwrap_or_default();
                if (matches!(following, b'?' | b'!') && second != b'=')
                    || (symbol(bytes, start)
                        && following == b'='
                        && !matches!(second, b'=' | b'~' | b'>'))
                {
                    position += 1;
                }
                token = &code[start..position];
            } else if byte.is_ascii_digit() {
                position += rest.iter().take_while(|&&byte| identifier(byte)).count();
                token = "0";
            } else if byte == b':'
                && !symbol(bytes, start)
                && (start == 0 || bytes[start - 1] != b':')
                && rest.get(1).is_some_and(|byte| OPERATORS.contains(byte))
            {
                position += 1 + rest[1..]
                    .iter()
                    .take_while(|byte| OPERATORS.contains(byte))
                    .count();
            } else if rest.starts_with(b"&&") || rest.starts_with(b"||") {
                token = &code[start..start + 2];
                position += 2;
                result.branches += 1;
            } else {
                position += 1;
                token = &code[start..position];
                match byte {
                    b'{' => result.events.push(Event::Open {
                        function_start: None,
                    }),
                    b'}' => result.events.push(Event::Close),
                    b'?' if start > 0
                        && bytes[start - 1].is_ascii_whitespace()
                        && rest.get(1).is_some_and(u8::is_ascii_whitespace) =>
                    {
                        result.branches += 1
                    }
                    _ => {}
                }
            }
            let keyword = ((byte.is_ascii_alphabetic())
                && (start == 0 || !b".:@$".contains(&bytes[start - 1]))
                && (bytes.get(position) != Some(&b':') || bytes.get(position + 1) == Some(&b':')))
            .then_some(token);
            match keyword {
                Some("def") => {
                    if let Some(name) = DEF_NAME.find(&code[position..]) {
                        let length = name.len();
                        position += length;
                        if bytes.get(position - 1) == Some(&b'=')
                            && !bytes
                                .get(position)
                                .is_none_or(|c| c.is_ascii_whitespace() || *c == b'(')
                        {
                            position -= 1;
                        }
                    }
                    let endless = ENDLESS
                        .find(&code[position..])
                        .is_some_and(|m| !code[position + m.end()..].starts_with(['=', '~', '>']));
                    result.events.push(if endless {
                        Event::InlineFunction
                    } else {
                        Event::Open {
                            function_start: Some(index),
                        }
                    });
                    token = "_";
                }
                Some("class" | "module" | "begin" | "case" | "for") => {
                    result.events.push(Event::Open {
                        function_start: None,
                    });
                    loop_awaits_do |= keyword == Some("for");
                }
                Some("if" | "unless" | "while" | "until") if statement(previous) => {
                    result.events.push(Event::Open {
                        function_start: None,
                    });
                    loop_awaits_do = matches!(keyword, Some("while" | "until"));
                }
                Some("do") => {
                    if loop_awaits_do {
                        loop_awaits_do = false;
                    } else {
                        result.events.push(Event::Open {
                            function_start: None,
                        });
                    }
                }
                Some("end") => result.events.push(Event::Close),
                Some("in") if previous.is_none() => result.branches += 1,
                _ => {}
            }
            if keyword.is_some_and(|word| BRANCHES.contains(&word)) {
                result.branches += 1;
            }
            previous = Some(if keyword.is_none() && starts_word(byte) {
                "_"
            } else {
                token
            });
        }
        self.loop_awaits_do = loop_awaits_do && (self.continued || b"|&,(+-*/<>=".contains(&last));
        Some(result)
    }
}

fn symbol(bytes: &[u8], start: usize) -> bool {
    start > 0 && bytes[start - 1] == b':' && (start < 2 || bytes[start - 2] != b':')
}

fn statement(previous: Option<&str>) -> bool {
    previous.is_none_or(|token| {
        STATEMENT_WORDS.contains(&token)
            || !identifier(token.as_bytes()[0]) && !b")] }@$".contains(&token.as_bytes()[0])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_heredoc_contents_and_modifier_conditions_but_keeps_interpolation() {
        let source = "def run\n  body = <<~DATA\n    end\n  DATA\n  notify if enabled\n  \"#{if ok then 1 end}\"\nend";
        let metrics = compute(source);
        assert_eq!(
            (
                metrics.functions,
                metrics.longest_function,
                metrics.nesting,
                metrics.branches
            ),
            (1, 7, 2, 2)
        );
    }

    #[test]
    fn keyword_names_and_operators_are_methods_not_blocks() {
        let source = "def self./(x)\n  x.end\n  :%\nend\ndef other(x) = x if x";
        let metrics = compute(source);
        assert_eq!(
            (metrics.functions, metrics.nesting, metrics.longest_function),
            (2, 1, 4)
        );
    }
}
