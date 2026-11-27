//! Preserve code tokens while masking comments and literals in brace languages.
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Dialect {
    Swift,
    Rust,
    C,
    Go,
    Php,
}

enum Frame {
    Literal {
        end: Vec<u8>,
        hashes: usize,
        raw: bool,
        multiline: bool,
        interpolation: bool,
    },
    Interpolation {
        open: u8,
        close: u8,
        depth: usize,
    },
    Heredoc {
        end: String,
        raw: bool,
    },
}

pub(super) struct Lexer {
    dialect: Dialect,
    frames: Vec<Frame>,
    conditionals: Vec<(bool, bool)>,
    comments: usize,
    continued_comment: bool,
    continued_directive: bool,
    html: bool,
}

static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(ifdef|ifndef|if|elifdef|elifndef|elif|else|endif)\b(.*)$").unwrap()
});
static DIRECTIVE_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/\*.*?\*/|//.*$").unwrap());
static PHP_HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^<<<[ \t]*(?:"([\pL_]\w*)"|'([\pL_]\w*)'|([\pL_]\w*))"#).unwrap()
});

impl Lexer {
    pub(super) fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            frames: Vec::new(),
            conditionals: Vec::new(),
            comments: 0,
            continued_comment: false,
            continued_directive: false,
            html: dialect == Dialect::Php,
        }
    }

    pub(super) fn strip(&mut self, line: &str) -> String {
        let line = line.trim_end_matches('\r');
        let c = self.dialect == Dialect::C;
        if c && self.continued_comment {
            self.continued_comment = line.ends_with('\\');
            return String::new();
        }
        let mut directive = c && self.continued_directive;
        if c && !directive
            && self.comments == 0
            && self.frames.is_empty()
            && let Some(tail) = line.trim_start().strip_prefix('#')
        {
            self.conditional(tail);
            directive = true;
        }
        let skipped = self.conditionals.iter().any(|(skip, _)| *skip);
        let bytes = line.as_bytes();
        let mut index = 0;
        let mut result = Vec::with_capacity(line.len());
        let mut pending_heredocs = Vec::new();
        if let Some(Frame::Heredoc { end, .. }) = self.frames.last() {
            let content = line.trim_start_matches([' ', '\t']);
            if let Some(rest) = content.strip_prefix(end.as_str())
                && !rest.as_bytes().first().is_some_and(|c| identifier(*c))
            {
                index = line.len() - content.len() + end.len();
                self.frames.pop();
            }
        }
        while index < bytes.len() {
            let tail = &bytes[index..];
            let ch = tail[0];
            if self.html {
                let Some(next) = php_tag(tail) else {
                    break;
                };
                self.html = false;
                index += next;
                continue;
            }
            if self.comments > 0 {
                if tail.starts_with(b"*/") {
                    self.comments -= 1;
                    index += 2;
                } else if matches!(self.dialect, Dialect::Swift | Dialect::Rust)
                    && tail.starts_with(b"/*")
                {
                    self.comments += 1;
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            match self.frames.last_mut() {
                Some(Frame::Literal {
                    end,
                    hashes,
                    raw,
                    interpolation,
                    ..
                }) => {
                    if tail.starts_with(end) {
                        index += end.len();
                        self.frames.pop();
                    } else if self.dialect == Dialect::Swift
                        && ch == b'\\'
                        && tail
                            .get(1..1 + *hashes)
                            .is_some_and(|rest| rest.iter().all(|&c| c == b'#'))
                    {
                        index += 1 + *hashes;
                        if bytes.get(index) == Some(&b'(') {
                            self.frames.push(Frame::Interpolation {
                                open: b'(',
                                close: b')',
                                depth: 0,
                            });
                        }
                        index += 1;
                    } else if self.dialect != Dialect::Swift && !*raw && ch == b'\\' {
                        index += 2;
                    } else if *interpolation && (tail.starts_with(b"{$") || tail.starts_with(b"${"))
                    {
                        self.frames.push(Frame::Interpolation {
                            open: b'{',
                            close: b'}',
                            depth: 0,
                        });
                        index += if ch == b'{' { 1 } else { 2 };
                    } else {
                        index += 1;
                    }
                    continue;
                }
                Some(Frame::Heredoc { raw, .. }) => {
                    if !*raw && (tail.starts_with(b"{$") || tail.starts_with(b"${")) {
                        self.frames.push(Frame::Interpolation {
                            open: b'{',
                            close: b'}',
                            depth: 0,
                        });
                        index += if ch == b'{' { 1 } else { 2 };
                    } else {
                        index += 1;
                    }
                    continue;
                }
                _ => {}
            }
            let php = self.dialect == Dialect::Php;
            if php && tail.starts_with(b"?>") {
                self.html = true;
                index += 2;
                continue;
            }
            if tail.starts_with(b"//") || (php && ch == b'#' && !tail.starts_with(b"#[")) {
                if php && let Some(close) = tail.windows(2).position(|part| part == b"?>") {
                    self.html = true;
                    index += close + 2;
                    continue;
                }
                self.continued_comment = c && line.ends_with('\\');
                break;
            }
            if tail.starts_with(b"/*") {
                self.comments = 1;
                index += 2;
                continue;
            }
            if let Some(Frame::Interpolation { open, close, depth }) = self.frames.last_mut() {
                if ch == *open {
                    *depth += 1;
                } else if ch == *close {
                    if *depth == 0 {
                        self.frames.pop();
                        index += 1;
                        continue;
                    }
                    *depth -= 1;
                }
            }
            if php
                && tail.starts_with(b"<<<")
                && let Some(rest) = line.get(index..)
                && let Some(captures) = PHP_HEREDOC.captures(rest)
                && let Some(end) = captures
                    .get(1)
                    .or_else(|| captures.get(2))
                    .or_else(|| captures.get(3))
            {
                pending_heredocs.push(Frame::Heredoc {
                    end: end.as_str().to_owned(),
                    raw: captures.get(2).is_some(),
                });
                index += captures.get(0).expect("complete match").len();
                result.push(b' ');
                continue;
            }
            if let Some((frame, skip)) = self.open(bytes, index) {
                self.frames.push(frame);
                index += skip;
                result.push(b' ');
                continue;
            }
            if ch == b'\''
                && let Some(consumed) = self.character(line, index)
            {
                index += consumed;
                result.push(b' ');
                continue;
            }
            result.push(ch);
            index += 1;
        }
        let swift = self.dialect == Dialect::Swift;
        if swift
            || (matches!(self.dialect, Dialect::C | Dialect::Go) && !(c && line.ends_with('\\')))
        {
            while self.frames.last().is_some_and(|frame| match frame {
                Frame::Literal { multiline, .. } => !multiline,
                Frame::Interpolation { .. } => swift,
                Frame::Heredoc { .. } => false,
            }) {
                self.frames.pop();
            }
        }
        self.frames.extend(pending_heredocs.into_iter().rev());
        if directive {
            self.continued_directive = line.ends_with('\\');
        }
        if directive || skipped {
            String::new()
        } else {
            String::from_utf8_lossy(&result).into_owned()
        }
    }

    fn conditional(&mut self, directive: &str) {
        let Some(capture) = DIRECTIVE.captures(directive) else {
            return;
        };
        let never = matches!(
            DIRECTIVE_COMMENT.replace_all(&capture[2], "").trim(),
            "0" | "(0)"
        );
        match &capture[1] {
            "if" | "ifdef" | "ifndef" => {
                let skip = &capture[1] == "if" && never;
                self.conditionals.push((skip, !skip));
            }
            "elif" | "elifdef" | "elifndef" => {
                if let Some((skip, taken)) = self.conditionals.last_mut() {
                    *skip = *taken || (&capture[1] == "elif" && never);
                    *taken |= !*skip;
                }
            }
            "else" => {
                if let Some((skip, taken)) = self.conditionals.last_mut() {
                    *skip = *taken;
                    *taken = true;
                }
            }
            "endif" => {
                self.conditionals.pop();
            }
            _ => {}
        }
    }

    fn open(&self, line: &[u8], index: usize) -> Option<(Frame, usize)> {
        let rest = &line[index..];
        let ch = rest[0];
        let (mut end, mut consumed, mut hashes, mut raw, mut multiline, mut interpolation) =
            (vec![b'"'], 1, 0, false, false, false);
        match self.dialect {
            Dialect::Swift => {
                if !matches!(ch, b'#' | b'"') || ch == b'#' && index > 0 && line[index - 1] == b'#'
                {
                    return None;
                }
                hashes = rest.iter().take_while(|&&b| b == b'#').count();
                if rest.get(hashes) != Some(&b'"') {
                    return None;
                }
                multiline = rest[hashes..].starts_with(b"\"\"\"");
                let quotes = if multiline { 3 } else { 1 };
                consumed = hashes + quotes;
                end = vec![b'"'; quotes];
                end.extend(std::iter::repeat_n(b'#', hashes));
            }
            Dialect::Rust => {
                if ch != b'"' {
                    if !matches!(ch, b'b' | b'c' | b'r')
                        || line[..index].last().is_some_and(|c| identifier(*c))
                    {
                        return None;
                    }
                    let prefix = usize::from(matches!(ch, b'b' | b'c'));
                    raw = rest.get(prefix) == Some(&b'r');
                    let starts = prefix + usize::from(raw);
                    if raw {
                        hashes = rest[starts..].iter().take_while(|&&b| b == b'#').count();
                    }
                    if rest.get(starts + hashes) != Some(&b'"') {
                        return None;
                    }
                    consumed = starts + hashes + 1;
                    end.extend(std::iter::repeat_n(b'#', hashes));
                }
                multiline = true;
            }
            Dialect::C => {
                if ch != b'"' {
                    return None;
                }
                if matches!(
                    preceding_word(&line[..index]),
                    b"R" | b"u8R" | b"LR" | b"uR" | b"UR"
                ) && let Some(paren) = rest.iter().take(18).position(|b| *b == b'(')
                    && paren > 0
                {
                    end = vec![b')'];
                    end.extend_from_slice(&rest[1..paren]);
                    end.push(b'"');
                    consumed = paren + 1;
                    raw = true;
                    multiline = true;
                }
            }
            Dialect::Go => match ch {
                b'"' => {}
                96 => {
                    end[0] = 96;
                    raw = true;
                    multiline = true;
                }
                _ => return None,
            },
            Dialect::Php => {
                if !matches!(ch, b'"' | b'\'' | 96) {
                    return None;
                }
                end[0] = ch;
                multiline = true;
                interpolation = ch != b'\'';
            }
        }
        Some((
            Frame::Literal {
                end,
                hashes,
                raw,
                multiline,
                interpolation,
            },
            consumed,
        ))
    }

    fn character(&self, line: &str, index: usize) -> Option<usize> {
        let bytes = line.as_bytes();
        let rest = &bytes[index + 1..];
        if self.dialect == Dialect::Rust {
            if rest.first() == Some(&b'\\') {
                return rest
                    .get(2..)?
                    .iter()
                    .position(|&b| b == b'\'')
                    .map(|end| end + 4);
            }
            let first = line.get(index + 1..)?.chars().next()?;
            let after = first.len_utf8();
            return (first != '\'' && rest.get(after) == Some(&b'\'')).then_some(after + 2);
        }
        if !matches!(self.dialect, Dialect::C | Dialect::Go) {
            return None;
        }
        let before = &bytes[..index];
        if before.last().is_some_and(|c| identifier(*c))
            && !matches!(preceding_word(before), b"L" | b"u" | b"U" | b"u8")
        {
            return None;
        }
        let mut escaped = false;
        for (position, &byte) in rest.iter().enumerate() {
            if !escaped && byte == b'\'' {
                return Some(position + 2);
            }
            escaped = !escaped && byte == b'\\';
        }
        None
    }
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

fn preceding_word(prefix: &[u8]) -> &[u8] {
    let start = prefix
        .iter()
        .rposition(|&byte| !identifier(byte))
        .map_or(0, |index| index + 1);
    &prefix[start..]
}

fn php_tag(tail: &[u8]) -> Option<usize> {
    for (index, prefix) in tail.windows(2).enumerate() {
        if prefix != b"<?" {
            continue;
        }
        let rest = &tail[index + 2..];
        if rest
            .get(..3)
            .is_some_and(|tag| tag.eq_ignore_ascii_case(b"php"))
            && !rest.get(3).is_some_and(|c| identifier(*c))
        {
            return Some(index + 5);
        }
        if rest.first() == Some(&b'=') {
            return Some(index + 3);
        }
        if rest.first().is_none_or(|c| matches!(c, b' ' | b'\t')) {
            return Some(index + 2);
        }
    }
    None
}
