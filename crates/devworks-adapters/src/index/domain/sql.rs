//! SQL: a port of the retained `SqlExtractor` (MySQL syntax normalized with the same expressions,
//! then a T-SQL syntax tree) and `SqlGraphVisitor`. The retained extractor walked a ScriptDom
//! (`TSql160Parser`) tree; this port scans the normalized text's T-SQL tokens for the statements
//! the visitor handled and reproduces what it emitted for them:
//!
//! - `CREATE TABLE`: a `Table` (`columns` with `name`/`type`/`nullable`/`is_primary_key`,
//!   `column_count`) and `QUERIES` foreign-key edges (`relationship`, `column` and
//!   `referenced_column` inline; `columns` and `referenced_columns` table-level);
//! - `ALTER TABLE ... ADD` foreign keys: `QUERIES` (`relationship`, `via: alter_table`);
//! - `CREATE`/`ALTER VIEW`: a `View` and `QUERIES` (`confidence_band: high`) to every named table
//!   its query reads;
//! - `CREATE`/`ALTER PROCEDURE`: a `StoredProcedure` (`parameters` with `name`/`type`/
//!   `is_output`, `parameter_count`), `QUERIES` to the tables its body reads or writes and `CALLS`
//!   to the procedures it executes (`confidence_band: high`);
//! - `CREATE`/`ALTER FUNCTION`: a `Function` (`parameters`, `parameter_count`,
//!   `kind: sql_function`); the retained visitor never walked a function body with the function
//!   as the current object, so a function contributes no `QUERIES`;
//! - `CREATE INDEX`: a `DEFINES` edge from the table to itself (`relationship: index`,
//!   `index_name`, `columns`, `is_unique`).
//!
//! Names are the retained `GetObjectName` (`name`, or `schema.name` for two to four parts);
//! temporary tables (`#t`) and table variables are never query targets. `CREATE OR ALTER`
//! statements derive from different ScriptDom classes than the ones the visitor overrode, so they
//! contribute no node (their nested statements still do), as in the retained output.
use super::{Lines, Out, props};
use devworks_core::ports::indexing::{ExtractFault, Span};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

// ---------------------------------------------------------------------------------------------
// MySQL normalization (`NormalizeMySqlSyntax`), keeping a map back to the source's offsets

struct Normalized {
    text: String,
    /// `map[i]`: the source offset of normalized byte `i` (one entry past the end).
    map: Vec<usize>,
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("a valid retained expression")
}

static REWRITES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (re(r"`([^`]*)`"), "[$1]"),
        (re(r"(?im)^\s*DELIMITER\s+\S+\s*$"), ""),
        (re(r"(?m)\$\$|//\s*$"), ";"),
        (re(r"(?i)\)\s*ENGINE\s*=\s*\w+[^;]*;"), ");"),
        (re(r"(?i)\bIF\s+NOT\s+EXISTS\b"), ""),
        (re(r"(?i)\bAUTO_INCREMENT\b"), "IDENTITY(1,1)"),
        (re(r"(?i)\bUNSIGNED\b"), ""),
        (
            re(r"(?i)\b(TINYINT|SMALLINT|MEDIUMINT|INT|BIGINT)\(\d+\)"),
            "$1",
        ),
        (re(r"(?i)\bBOOL(?:EAN)?\b"), "BIT"),
        (re(r"(?i)\bENUM\s*\([^)]*\)"), "VARCHAR(255)"),
        (re(r"(?i)\bLONGTEXT\b"), "NVARCHAR(MAX)"),
        (re(r"(?i)\bMEDIUMTEXT\b"), "NVARCHAR(MAX)"),
        (re(r"(?i)\bTINYTEXT\b"), "NVARCHAR(255)"),
        (re(r"(?i)\b(?:LONG|MEDIUM|TINY)?BLOB\b"), "VARBINARY(MAX)"),
        (re(r"(?i)\bDOUBLE\b"), "FLOAT"),
        (re(r"(?i)\bON\s+UPDATE\s+CURRENT_TIMESTAMP\b"), ""),
        (
            re(r"(?i)\bDEFAULT\s+CURRENT_TIMESTAMP\b"),
            "DEFAULT GETDATE()",
        ),
        (
            re(r"(?i)\b(DEFAULT\s+)?(CHARACTER\s+SET|CHARSET)\s*=?\s*\w+"),
            "",
        ),
        (re(r"(?i)\bCOLLATE\s*=?\s*\w+"), ""),
        (re(r"(?i)\bAUTO_INCREMENT\s*=\s*\d+"), ""),
    ]
});

fn rewrite(n: &mut Normalized, r: &Regex, replacement: &str) {
    let mut text = String::with_capacity(n.text.len());
    let mut map = Vec::with_capacity(n.map.len());
    let mut last = 0;
    for c in r.captures_iter(&n.text) {
        let m = c.get(0).expect("a match");
        text.push_str(&n.text[last..m.start()]);
        map.extend_from_slice(&n.map[last..m.start()]);
        let before = text.len();
        c.expand(replacement, &mut text);
        map.extend(std::iter::repeat_n(n.map[m.start()], text.len() - before));
        last = m.end();
    }
    text.push_str(&n.text[last..]);
    map.extend_from_slice(&n.map[last..]);
    n.text = text;
    n.map = map;
}

/// `#` line comments (not `#temp` names) become `--` comments: the retained
/// `(?<=^|\s)#(?![A-Za-z_])(.*)$` (multiline) replaced with `-- $1`.
fn hash_comments(n: &mut Normalized) {
    let src = std::mem::take(&mut n.text);
    let old = std::mem::take(&mut n.map);
    let mut prev: Option<char> = None;
    let mut chars = src.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let after_boundary = prev.is_none_or(|p| p == '\n' || p.is_whitespace());
        let next_ok = chars
            .peek()
            .is_none_or(|(_, nx)| !(nx.is_ascii_alphabetic() || *nx == '_'));
        if c == '#' && after_boundary && next_ok {
            n.text.push_str("-- ");
            n.map.extend([old[i]; 3]);
            // The rest of the line is kept as the comment's text.
            while let Some((j, d)) = chars.peek().copied() {
                if d == '\n' {
                    break;
                }
                n.text.push(d);
                n.map.extend(std::iter::repeat_n(old[j], d.len_utf8()));
                chars.next();
                prev = Some(d);
            }
            continue;
        }
        n.text.push(c);
        n.map.extend((0..c.len_utf8()).map(|k| old[i + k]));
        prev = Some(c);
    }
    n.map.push(old[src.len()]);
}

fn normalize(src: &str) -> Normalized {
    let mut n = Normalized {
        text: src.to_owned(),
        map: (0..=src.len()).collect(),
    };
    hash_comments(&mut n);
    for (r, rep) in REWRITES.iter() {
        rewrite(&mut n, r, rep);
    }
    n
}

// ---------------------------------------------------------------------------------------------
// T-SQL tokens

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum T {
    Word,
    Quoted,
    Str,
    Num,
    Punct(u8),
}
#[derive(Clone, Copy, Debug)]
struct Tok {
    t: T,
    s: usize,
    e: usize,
    line: usize,
}

fn word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '@' || c == '#'
}
fn word_part(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$')
}

fn tokenize(text: &str) -> Vec<Tok> {
    let b = text.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line = 1usize;
    let advance_lines = |from: usize, to: usize, line: &mut usize| {
        *line += b[from..to].iter().filter(|c| **c == b'\n').count();
    };
    while i < b.len() {
        let c = text[i..].chars().next().unwrap_or(' ');
        let start = i;
        if c.is_whitespace() {
            i += c.len_utf8();
            advance_lines(start, i, &mut line);
            continue;
        }
        if text[i..].starts_with("--") {
            i = text[i..].find('\n').map_or(b.len(), |n| i + n);
            continue;
        }
        if text[i..].starts_with("/*") {
            let mut depth = 0usize;
            while i < b.len() {
                if text[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if text[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            advance_lines(start, i.min(b.len()), &mut line);
            continue;
        }
        let tok_line = line;
        let (t, end) = if c == '\'' || ((c == 'N' || c == 'n') && b.get(i + 1) == Some(&b'\'')) {
            let mut j = if c == '\'' { i + 1 } else { i + 2 };
            loop {
                match b.get(j) {
                    None => break,
                    Some(b'\'') if b.get(j + 1) == Some(&b'\'') => j += 2,
                    Some(b'\'') => {
                        j += 1;
                        break;
                    }
                    _ => j += 1,
                }
            }
            (T::Str, j)
        } else if c == '[' || c == '"' {
            let close = if c == '[' { b']' } else { b'"' };
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => break,
                    Some(x) if *x == close && b.get(j + 1) == Some(&close) => j += 2,
                    Some(x) if *x == close => {
                        j += 1;
                        break;
                    }
                    _ => j += 1,
                }
            }
            (T::Quoted, j)
        } else if word_start(c) {
            let mut j = i + c.len_utf8();
            while let Some(d) = text[j..].chars().next() {
                if !word_part(d) {
                    break;
                }
                j += d.len_utf8();
            }
            (T::Word, j)
        } else if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.') {
                j += 1;
            }
            (T::Num, j)
        } else if c == '$' {
            // `$action` and money literals.
            let mut j = i + 1;
            while let Some(d) = text[j..].chars().next() {
                if !word_part(d) && d != '.' {
                    break;
                }
                j += d.len_utf8();
            }
            (T::Word, j)
        } else {
            (
                T::Punct(if c.is_ascii() { c as u8 } else { b'?' }),
                i + c.len_utf8(),
            )
        };
        let end = end.min(b.len());
        advance_lines(start, end, &mut line);
        toks.push(Tok {
            t,
            s: start,
            e: end,
            line: tok_line,
        });
        i = end;
    }
    toks
}

/// Keywords that begin a statement (where a list or clause of the previous one ends).
const STATEMENT_START: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "SET",
    "DECLARE",
    "IF",
    "ELSE",
    "WHILE",
    "BEGIN",
    "END",
    "RETURN",
    "EXEC",
    "EXECUTE",
    "PRINT",
    "RAISERROR",
    "THROW",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "WITH",
    "OPEN",
    "CLOSE",
    "FETCH",
    "DEALLOCATE",
    "COMMIT",
    "ROLLBACK",
    "WAITFOR",
    "GRANT",
    "REVOKE",
    "DENY",
    "USE",
    "BREAK",
    "CONTINUE",
    "GOTO",
];
/// Clauses that end a `FROM` list.
const FROM_END: &[&str] = &[
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "OPTION",
    "FOR",
    "WINDOW",
];
/// Reserved words that never name a table.
const RESERVED: &[&str] = &[
    "SELECT",
    "SET",
    "VALUES",
    "WHERE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ON",
    "AS",
    "DEFAULT",
    "FROM",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "GROUP",
    "ORDER",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "INTO",
    "TOP",
    "OUTPUT",
    "OPTION",
    "WITH",
    "BEGIN",
    "DECLARE",
    "IF",
    "WHILE",
    "RETURN",
    "EXEC",
    "EXECUTE",
    "PRINT",
    "NULL",
    "NOT",
    "AND",
    "OR",
    "EXISTS",
    "CASE",
    "STATISTICS",
    "USING",
    "APPLY",
    "OPENQUERY",
    "OPENROWSET",
    "OPENJSON",
    "OPENXML",
    "OPENDATASOURCE",
    "CONTAINSTABLE",
    "FREETEXTTABLE",
    "TABLE",
    "DISTINCT",
    "ALL",
    "CREATE",
    "ALTER",
    "DROP",
    "GO",
];
/// Table hints of the deprecated `FROM t (NOLOCK)` form.
const TABLE_HINTS: &[&str] = &[
    "NOLOCK",
    "READUNCOMMITTED",
    "READCOMMITTED",
    "REPEATABLEREAD",
    "SERIALIZABLE",
    "HOLDLOCK",
    "UPDLOCK",
    "ROWLOCK",
    "PAGLOCK",
    "TABLOCK",
    "TABLOCKX",
    "XLOCK",
    "NOWAIT",
    "READPAST",
];

struct Param {
    name: String,
    ty: String,
    output: bool,
}

pub(crate) fn extract(source: &[u8], out: &mut Out<'_>) -> Result<(), ExtractFault> {
    out.text_scanner();
    let original = String::from_utf8_lossy(source);
    let n = normalize(&original);
    let toks = tokenize(&n.text);
    let lines = Lines::new(&original);
    let mut s = Sql {
        text: &n.text,
        map: &n.map,
        toks,
        lines: &lines,
        out,
    };
    // Batches end at `GO` on its own line.
    let mut start = 0;
    let len = s.toks.len();
    let mut i = 0;
    while i <= len {
        let go = i < len && s.kw(i, "GO") && (i == 0 || s.toks[i - 1].line < s.toks[i].line);
        if i == len || go {
            s.statements(start, i, None)?;
            start = i + 1;
            // `GO 5`: a repeat count.
            if go
                && start < len
                && s.toks[start].t == T::Num
                && s.toks[start].line == s.toks[i].line
            {
                start += 1;
            }
            i = start;
            continue;
        }
        i += 1;
    }
    Ok(())
}

struct Sql<'c, 'o, 'a> {
    text: &'c str,
    map: &'c [usize],
    toks: Vec<Tok>,
    lines: &'c Lines,
    out: &'o mut Out<'a>,
}

#[derive(Default)]
struct Scan {
    depth: usize,
    from_depth: Option<usize>,
    output_depth: Option<usize>,
}

impl Sql<'_, '_, '_> {
    fn txt(&self, i: usize) -> &str {
        self.toks.get(i).map_or("", |t| &self.text[t.s..t.e])
    }
    fn kw(&self, i: usize, k: &str) -> bool {
        self.toks.get(i).is_some_and(|t| t.t == T::Word) && self.txt(i).eq_ignore_ascii_case(k)
    }
    fn kw_in(&self, i: usize, ks: &[&str]) -> bool {
        ks.iter().any(|k| self.kw(i, k))
    }
    fn punct(&self, i: usize, c: u8) -> bool {
        self.toks.get(i).is_some_and(|t| t.t == T::Punct(c))
    }
    /// An identifier's value (brackets and quotes removed).
    fn ident(&self, i: usize) -> Option<String> {
        let t = self.toks.get(i)?;
        let raw = &self.text[t.s..t.e];
        match t.t {
            T::Word => Some(raw.to_owned()),
            T::Quoted => {
                let (open, close) = if raw.starts_with('[') {
                    ("[", "]")
                } else {
                    ("\"", "\"")
                };
                let inner = raw.strip_prefix(open).unwrap_or(raw);
                let inner = inner.strip_suffix(close).unwrap_or(inner);
                Some(inner.replace(&format!("{close}{close}"), close))
            }
            _ => None,
        }
    }
    /// A multi-part name `a.b.c` (empty parts allowed: `db..t`) from `i`; the parts and the next
    /// token.
    fn name_chain(&self, i: usize, end: usize) -> Option<(Vec<String>, usize)> {
        if i >= end {
            return None;
        }
        let mut parts = vec![self.ident(i)?];
        let mut k = i + 1;
        while k < end && self.punct(k, b'.') {
            if self.punct(k + 1, b'.') {
                parts.push(String::new());
                k += 1;
                continue;
            }
            match self.ident(k + 1).filter(|_| k + 1 < end) {
                Some(p) => {
                    parts.push(p);
                    k += 2;
                }
                None => break,
            }
        }
        Some((parts, k))
    }
    /// The retained `GetObjectName`.
    fn object_name(parts: &[String]) -> String {
        match parts.len() {
            1 => parts[0].clone(),
            2 => format!("{}.{}", parts[0], parts[1]),
            3 => format!("{}.{}", parts[1], parts[2]),
            4 => format!("{}.{}", parts[2], parts[3]),
            _ => parts.last().cloned().unwrap_or_default(),
        }
    }
    /// The token after the `)` matching the `(` at `i` (or `end`).
    fn after_close(&self, i: usize, end: usize) -> usize {
        let mut depth = 0usize;
        let mut k = i;
        while k < end {
            if self.punct(k, b'(') {
                depth += 1;
            } else if self.punct(k, b')') {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k + 1;
                }
            }
            k += 1;
        }
        end
    }
    /// The elements of the parenthesized list at `open`, split at its top-level commas.
    fn list(&self, open: usize, end: usize) -> Vec<(usize, usize)> {
        let close = self.after_close(open, end).saturating_sub(1);
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut start = open + 1;
        for k in open + 1..close.max(open + 1) {
            if self.punct(k, b'(') {
                depth += 1;
            } else if self.punct(k, b')') {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && self.punct(k, b',') {
                out.push((start, k));
                start = k + 1;
            }
        }
        if start < close {
            out.push((start, close));
        }
        out
    }
    /// The identifier of each element of a list (the last part of a name; `col ASC` → `col`).
    fn list_names(&self, open: usize, end: usize) -> Vec<String> {
        self.list(open, end)
            .into_iter()
            .filter_map(|(a, b)| self.name_chain(a, b).and_then(|(p, _)| p.last().cloned()))
            .collect()
    }
    /// The source span of tokens `a..=b`.
    fn span(&self, a: usize, b: usize) -> Span {
        let s = self.toks.get(a).map_or(0, |t| t.s);
        let e = self.toks.get(b).map_or(s, |t| t.e);
        self.lines.span(self.map[s], self.map[e])
    }
    /// The first token after a statement starting at `from`: a top-level `;`, a statement
    /// keyword (not one of an `ON DELETE`/`ON UPDATE` action) or `end`.
    fn statement_end(&self, from: usize, end: usize, keywords: &[&str]) -> usize {
        let mut depth = 0usize;
        let mut k = from;
        while k < end {
            if self.punct(k, b'(') {
                depth += 1;
            } else if self.punct(k, b')') {
                depth = depth.saturating_sub(1);
            } else if depth == 0 {
                if self.punct(k, b';') {
                    return k;
                }
                let action = k > 0
                    && (self.kw(k - 1, "ON")
                        || (self.kw(k, "SET") && self.kw_in(k - 1, &["DELETE", "UPDATE"])));
                if self.kw_in(k, keywords) && !action {
                    return k;
                }
            }
            k += 1;
        }
        end
    }

    /// A data type from `k`: its source text and the next token.
    fn data_type(&self, k: usize, end: usize) -> Option<(String, usize)> {
        let (parts, mut n) = self.name_chain(k, end)?;
        if parts.len() == 1 && parts[0].eq_ignore_ascii_case("NATIONAL") {
            if self.kw_in(n, &["CHAR", "CHARACTER"]) {
                n += 1;
            }
            if self.kw(n, "VARYING") {
                n += 1;
            }
        } else if self.kw_in(n, &["VARYING", "PRECISION"]) {
            n += 1;
        }
        if n < end && self.punct(n, b'(') {
            n = self.after_close(n, end);
        }
        let s = self.toks[k].s;
        let e = self.toks[n - 1].e;
        Some((self.text[s..e].trim().to_owned(), n))
    }

    /// Procedure or function parameters from `k` (parenthesized or not); the next token.
    fn parameters(&self, k: usize, end: usize) -> (Vec<Param>, usize) {
        let mut ps = Vec::new();
        let paren = self.punct(k, b'(');
        let (mut k, limit) = if paren {
            (k + 1, self.after_close(k, end).saturating_sub(1))
        } else {
            (k, end)
        };
        while k < limit {
            let Some(t) = self.toks.get(k) else { break };
            let name = self.txt(k);
            if t.t != T::Word || !name.starts_with('@') {
                break;
            }
            let name = name.to_owned();
            k += 1;
            if self.kw(k, "AS") {
                k += 1;
            }
            let Some((ty, next)) = self.data_type(k, limit) else {
                break;
            };
            k = next;
            let mut output = false;
            loop {
                if self.kw(k, "VARYING") || self.kw(k, "READONLY") || self.kw(k, "NULL") {
                    k += 1;
                } else if self.kw_in(k, &["OUT", "OUTPUT"]) {
                    output = true;
                    k += 1;
                } else if self.punct(k, b'=') {
                    k += 1;
                    if self.punct(k, b'-') || self.punct(k, b'+') {
                        k += 1;
                    }
                    k = if self.punct(k, b'(') {
                        self.after_close(k, limit)
                    } else {
                        k + 1
                    };
                } else {
                    break;
                }
            }
            ps.push(Param { name, ty, output });
            if k < limit && self.punct(k, b',') {
                k += 1;
            } else {
                break;
            }
        }
        if paren {
            k = limit + 1;
        }
        (ps, k)
    }

    /// The statements of `start..end`, with `current` the object whose body they are.
    fn statements(
        &mut self,
        start: usize,
        end: usize,
        current: Option<&str>,
    ) -> Result<(), ExtractFault> {
        let mut st = Scan::default();
        let mut i = start;
        while i < end {
            self.out.tick()?;
            if let Some(next) = self.statement(i, end, current)? {
                st = Scan::default();
                i = next.max(i + 1);
                continue;
            }
            if let Some(cur) = current {
                self.references(i, end, cur, &mut st);
            }
            i += 1;
        }
        Ok(())
    }

    /// A `CREATE`/`ALTER` statement the retained visitor handled, starting at `i`; the token after
    /// it.
    fn statement(
        &mut self,
        i: usize,
        end: usize,
        current: Option<&str>,
    ) -> Result<Option<usize>, ExtractFault> {
        let create = self.kw(i, "CREATE");
        if !create && !self.kw(i, "ALTER") {
            return Ok(None);
        }
        let mut j = i + 1;
        let mut or_alter = false;
        if create && self.kw(j, "OR") && self.kw(j + 1, "ALTER") {
            or_alter = true;
            j += 2;
        }
        let emit = !or_alter;
        if self.kw(j, "TABLE") {
            return Ok(match (create, or_alter) {
                (true, false) => self.create_table(i, j + 1, end),
                (false, _) => self.alter_table(j + 1, end),
                _ => None,
            });
        }
        if self.kw(j, "VIEW") {
            return self.view(i, j + 1, end, emit).map(Some);
        }
        if self.kw_in(j, &["PROC", "PROCEDURE"]) {
            return self.procedure(i, j + 1, end, emit).map(Some);
        }
        if self.kw(j, "FUNCTION") {
            return Ok(Some(self.function(i, j + 1, end, emit)));
        }
        if self.kw(j, "TRIGGER") {
            // Not a visited statement; its body's nested statements still are.
            let mut k = j + 1;
            while k < end && !self.kw(k, "AS") {
                k += 1;
            }
            self.statements(k + 1, end, None)?;
            return Ok(Some(end));
        }
        if create {
            let mut k = j;
            let unique = self.kw(k, "UNIQUE");
            if unique {
                k += 1;
            }
            if self.kw_in(k, &["CLUSTERED", "NONCLUSTERED"]) {
                k += 1;
            }
            if self.kw(k, "INDEX") {
                return Ok(self.create_index(k + 1, end, unique));
            }
        }
        let _ = current;
        Ok(None)
    }

    fn create_table(&mut self, i: usize, j: usize, end: usize) -> Option<usize> {
        let (parts, k) = self.name_chain(j, end)?;
        if !self.punct(k, b'(') {
            return None;
        }
        let name = Self::object_name(&parts);
        let close = self.after_close(k, end);
        let elements = self.list(k, end);
        const CONSTRAINT_START: &[&str] = &[
            "CONSTRAINT",
            "PRIMARY",
            "FOREIGN",
            "UNIQUE",
            "CHECK",
            "INDEX",
            "KEY",
            "FULLTEXT",
            "SPATIAL",
        ];
        let is_constraint = |s: &Self, a: usize| {
            s.kw_in(a, CONSTRAINT_START) || (s.kw(a, "PERIOD") && s.kw(a + 1, "FOR"))
        };
        // Primary-key columns of the table-level constraints.
        let mut pk: Vec<String> = Vec::new();
        let mut table_fks: Vec<(Vec<String>, String, Vec<String>)> = Vec::new();
        for &(a, b) in &elements {
            if !is_constraint(self, a) {
                continue;
            }
            let mut c = a;
            if self.kw(c, "CONSTRAINT") {
                c += 2;
            }
            if self.kw(c, "PRIMARY") && self.kw(c + 1, "KEY") {
                let mut o = c + 2;
                while o < b && !self.punct(o, b'(') {
                    o += 1;
                }
                if o < b {
                    pk.extend(self.list_names(o, b + 1));
                }
            } else if self.kw(c, "FOREIGN") && self.kw(c + 1, "KEY") && self.punct(c + 2, b'(') {
                let cols = self.list_names(c + 2, b + 1);
                let r = self.after_close(c + 2, b);
                if self.kw(r, "REFERENCES")
                    && let Some((rp, rn)) = self.name_chain(r + 1, b)
                {
                    let refs = if self.punct(rn, b'(') {
                        self.list_names(rn, b + 1)
                    } else {
                        Vec::new()
                    };
                    table_fks.push((cols, Self::object_name(&rp), refs));
                }
            }
        }
        let mut columns: Vec<Value> = Vec::new();
        for &(a, b) in &elements {
            if is_constraint(self, a) {
                continue;
            }
            let Some(col) = self.ident(a) else { continue };
            let (ty, mut k2) = if self.kw(a + 1, "AS") {
                ("unknown".to_owned(), a + 1)
            } else {
                self.data_type(a + 1, b)
                    .unwrap_or_else(|| ("unknown".to_owned(), a + 1))
            };
            let mut not_null = false;
            let mut inline_pk = false;
            while k2 < b {
                if self.punct(k2, b'(') {
                    k2 = self.after_close(k2, b);
                    continue;
                }
                if self.kw(k2, "NOT") && self.kw(k2 + 1, "NULL") {
                    not_null = true;
                    k2 += 2;
                    continue;
                }
                if self.kw(k2, "PRIMARY") && self.kw(k2 + 1, "KEY") {
                    inline_pk = true;
                    k2 += 2;
                    continue;
                }
                if self.kw(k2, "REFERENCES")
                    && let Some((rp, rn)) = self.name_chain(k2 + 1, b)
                {
                    let referenced = if self.punct(rn, b'(') {
                        self.list_names(rn, b + 1).into_iter().next()
                    } else {
                        None
                    };
                    self.out.edge(
                        "QUERIES",
                        &name,
                        &Self::object_name(&rp),
                        props(&[
                            ("relationship", json!("foreign_key")),
                            ("column", json!(col)),
                            ("referenced_column", json!(referenced.unwrap_or_default())),
                        ]),
                    );
                    k2 = rn;
                    continue;
                }
                k2 += 1;
            }
            if inline_pk {
                pk.push(col.clone());
            }
            let mut info = Map::new();
            info.insert("name".into(), json!(col));
            info.insert("type".into(), json!(ty));
            info.insert("nullable".into(), json!(!not_null));
            if pk.iter().any(|p| p.eq_ignore_ascii_case(&col)) {
                info.insert("is_primary_key".into(), json!(true));
            }
            columns.push(Value::Object(info));
        }
        for (cols, target, refs) in table_fks {
            self.out.edge(
                "QUERIES",
                &name,
                &target,
                props(&[
                    ("relationship", json!("foreign_key")),
                    ("columns", json!(cols.join(", "))),
                    ("referenced_columns", json!(refs.join(", "))),
                ]),
            );
        }
        let count = columns.len();
        let span = self.span(i, close.saturating_sub(1));
        self.out.node(
            "Table",
            &name,
            &name,
            span,
            None,
            props(&[
                ("columns", Value::Array(columns)),
                ("column_count", json!(count)),
            ]),
        );
        Some(close)
    }

    fn alter_table(&mut self, j: usize, end: usize) -> Option<usize> {
        let (parts, mut k) = self.name_chain(j, end)?;
        if self.kw(k, "WITH") && self.kw_in(k + 1, &["CHECK", "NOCHECK"]) {
            k += 2;
        }
        if !self.kw(k, "ADD") {
            return None;
        }
        let table = Self::object_name(&parts);
        let stmt_end = self.statement_end(k + 1, end, STATEMENT_START);
        let mut depth = 0usize;
        let mut start = k + 1;
        let mut elements = Vec::new();
        for x in k + 1..stmt_end {
            if self.punct(x, b'(') {
                depth += 1;
            } else if self.punct(x, b')') {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && self.punct(x, b',') {
                elements.push(start);
                start = x + 1;
            }
        }
        elements.push(start);
        for a in elements {
            let mut c = a;
            if self.kw(c, "CONSTRAINT") {
                c += 2;
            }
            if !(self.kw(c, "FOREIGN") && self.kw(c + 1, "KEY") && self.punct(c + 2, b'(')) {
                continue;
            }
            let r = self.after_close(c + 2, stmt_end);
            if self.kw(r, "REFERENCES")
                && let Some((rp, _)) = self.name_chain(r + 1, stmt_end)
            {
                self.out.edge(
                    "QUERIES",
                    &table,
                    &Self::object_name(&rp),
                    props(&[
                        ("relationship", json!("foreign_key")),
                        ("via", json!("alter_table")),
                    ]),
                );
            }
        }
        Some(stmt_end)
    }

    fn view(&mut self, i: usize, j: usize, end: usize, emit: bool) -> Result<usize, ExtractFault> {
        let Some((parts, mut k)) = self.name_chain(j, end) else {
            return Ok(j);
        };
        if self.punct(k, b'(') {
            k = self.after_close(k, end);
        }
        while k < end && !self.kw(k, "AS") {
            k += 1;
        }
        let body = k + 1;
        let body_end = self.statement_end(
            body,
            end,
            &[
                "CREATE", "ALTER", "DROP", "GRANT", "REVOKE", "DENY", "EXEC", "EXECUTE", "INSERT",
                "UPDATE", "DELETE", "MERGE", "SET", "DECLARE", "PRINT", "IF", "WHILE", "BEGIN",
                "USE", "TRUNCATE", "RETURN",
            ],
        );
        let name = Self::object_name(&parts);
        if emit {
            let span = self.span(i, body_end.saturating_sub(1));
            self.out.node("View", &name, &name, span, None, Map::new());
            self.statements(body, body_end, Some(&name))?;
        } else {
            self.statements(body, body_end, None)?;
        }
        Ok(body_end)
    }

    fn procedure(
        &mut self,
        i: usize,
        j: usize,
        end: usize,
        emit: bool,
    ) -> Result<usize, ExtractFault> {
        let Some((parts, mut k)) = self.name_chain(j, end) else {
            return Ok(j);
        };
        if self.punct(k, b';') && self.toks.get(k + 1).is_some_and(|t| t.t == T::Num) {
            k += 2;
        }
        let (params, mut k) = self.parameters(k, end);
        if self.kw(k, "WITH") {
            k += 1;
            while k < end && !(self.kw(k, "AS") && !self.kw_in(k - 1, &["EXEC", "EXECUTE"])) {
                k += 1;
            }
        }
        if self.kw(k, "FOR") && self.kw(k + 1, "REPLICATION") {
            k += 2;
        }
        let body = if self.kw(k, "AS") { k + 1 } else { k };
        let name = Self::object_name(&parts);
        if emit {
            let list: Vec<Value> = params
                .iter()
                .map(|p| json!({"name": p.name, "type": p.ty, "is_output": p.output}))
                .collect();
            let count = list.len();
            let span = self.span(i, end.saturating_sub(1));
            self.out.node(
                "StoredProcedure",
                &name,
                &name,
                span,
                None,
                props(&[
                    ("parameters", Value::Array(list)),
                    ("parameter_count", json!(count)),
                ]),
            );
            self.statements(body, end, Some(&name))?;
        } else {
            self.statements(body, end, None)?;
        }
        Ok(end)
    }

    fn function(&mut self, i: usize, j: usize, end: usize, emit: bool) -> usize {
        let Some((parts, k)) = self.name_chain(j, end) else {
            return j;
        };
        let (params, k) = if self.punct(k, b'(') {
            self.parameters(k, end)
        } else {
            (Vec::new(), k)
        };
        // The function ends with its `BEGIN ... END` body, or at the end of its statement.
        let mut f_end = end;
        let mut depth = 0usize;
        let mut x = k;
        while x < end {
            if self.punct(x, b'(') {
                depth += 1;
            } else if self.punct(x, b')') {
                depth = depth.saturating_sub(1);
            } else if depth == 0 {
                if self.kw(x, "BEGIN") {
                    f_end = self.block_end(x, end);
                    break;
                }
                if self.punct(x, b';') || (x > k && self.kw_in(x, &["CREATE", "ALTER"])) {
                    f_end = x;
                    break;
                }
            }
            x += 1;
        }
        if emit {
            let name = Self::object_name(&parts);
            let list: Vec<Value> = params
                .iter()
                .map(|p| json!({"name": p.name, "type": p.ty}))
                .collect();
            let count = list.len();
            let span = self.span(i, f_end.saturating_sub(1));
            self.out.node(
                "Function",
                &name,
                &name,
                span,
                None,
                props(&[
                    ("parameters", Value::Array(list)),
                    ("parameter_count", json!(count)),
                    ("kind", json!("sql_function")),
                ]),
            );
        }
        f_end
    }

    /// The token after the `END` closing the `BEGIN` at `b` (`CASE ... END` nests).
    fn block_end(&self, b: usize, end: usize) -> usize {
        let mut depth = 0usize;
        for x in b..end {
            let begin = self.kw(x, "BEGIN")
                && !self.kw_in(
                    x + 1,
                    &[
                        "TRAN",
                        "TRANSACTION",
                        "DISTRIBUTED",
                        "DIALOG",
                        "CONVERSATION",
                    ],
                );
            if begin || self.kw(x, "CASE") {
                depth += 1;
            } else if self.kw(x, "END") {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return x + 1;
                }
            }
        }
        end
    }

    fn create_index(&mut self, k: usize, end: usize, unique: bool) -> Option<usize> {
        let (index_name, mut k) = if self.kw(k, "ON") {
            ("unnamed".to_owned(), k)
        } else {
            (self.ident(k)?, k + 1)
        };
        if !self.kw(k, "ON") {
            return None;
        }
        k += 1;
        let (parts, k) = self.name_chain(k, end)?;
        let table = Self::object_name(&parts);
        let (columns, next) = if self.punct(k, b'(') {
            (self.list_names(k, end), self.after_close(k, end))
        } else {
            (Vec::new(), k)
        };
        self.out.edge(
            "DEFINES",
            &table,
            &table,
            props(&[
                ("relationship", json!("index")),
                ("index_name", json!(index_name)),
                ("columns", json!(columns.join(", "))),
                ("is_unique", json!(unique)),
            ]),
        );
        Some(next)
    }

    /// Table references and procedure calls at token `i` of the body of `current`.
    fn references(&mut self, i: usize, end: usize, current: &str, st: &mut Scan) {
        if self.punct(i, b'(') {
            st.depth += 1;
            return;
        }
        if self.punct(i, b')') {
            st.depth = st.depth.saturating_sub(1);
            if st.from_depth.is_some_and(|d| d > st.depth) {
                st.from_depth = None;
            }
            if st.output_depth.is_some_and(|d| d > st.depth) {
                st.output_depth = None;
            }
            return;
        }
        if self.punct(i, b';') {
            st.from_depth = None;
            st.output_depth = None;
            return;
        }
        if self.punct(i, b',') {
            if st.from_depth == Some(st.depth) {
                self.table_source(i + 1, end, current, false);
            }
            return;
        }
        if st.from_depth == Some(st.depth) && self.kw_in(i, FROM_END) {
            st.from_depth = None;
        }
        if self.kw_in(i, STATEMENT_START) && !self.kw(i, "SET") {
            st.from_depth = None;
            st.output_depth = None;
        }
        if self.kw(i, "FROM") {
            let fetch = (1..=3).any(|d| i >= d && self.kw(i - d, "FETCH"));
            if i > 0 && self.kw(i - 1, "DISTINCT") || fetch {
                return;
            }
            st.from_depth = Some(st.depth);
            self.table_source(i + 1, end, current, false);
        } else if self.kw_in(i, &["JOIN", "APPLY", "USING"]) {
            self.table_source(i + 1, end, current, false);
        } else if self.kw(i, "UPDATE") {
            if i > 0 && self.kw_in(i - 1, &["FOR", "ON"]) || self.kw(i + 1, "STATISTICS") {
                return;
            }
            let k = self.skip_top(i + 1, end);
            self.table_source(k, end, current, false);
        } else if self.kw(i, "INSERT") {
            if i > 0 && self.kw(i - 1, "BULK") {
                return;
            }
            let mut k = self.skip_top(i + 1, end);
            if self.kw(k, "INTO") {
                k += 1;
            }
            self.table_source(k, end, current, true);
        } else if self.kw(i, "DELETE") {
            if i > 0 && self.kw(i - 1, "ON") {
                return;
            }
            let k = self.skip_top(i + 1, end);
            if !self.kw(k, "FROM") {
                self.table_source(k, end, current, false);
            }
        } else if self.kw(i, "MERGE") {
            let mut k = self.skip_top(i + 1, end);
            if self.kw(k, "INTO") {
                k += 1;
            }
            self.table_source(k, end, current, false);
        } else if self.kw(i, "OUTPUT") {
            if self.kw_in(i + 1, &["INSERTED", "DELETED"]) || self.txt(i + 1).starts_with('$') {
                st.output_depth = Some(st.depth);
            }
        } else if self.kw(i, "INTO") && st.output_depth == Some(st.depth) {
            st.output_depth = None;
            self.table_source(i + 1, end, current, false);
        } else if self.kw_in(i, &["EXEC", "EXECUTE"]) {
            self.exec(i, end, current);
        }
    }

    fn skip_top(&self, k: usize, end: usize) -> usize {
        if self.kw(k, "TOP") {
            let k = k + 1;
            let k = if self.punct(k, b'(') {
                self.after_close(k, end)
            } else {
                k + 1
            };
            if self.kw(k, "PERCENT") { k + 1 } else { k }
        } else {
            k
        }
    }

    /// A `NamedTableReference` at `j` (not a derived table, a table variable, a table-valued
    /// function or a temporary table).
    fn table_source(&mut self, j: usize, end: usize, current: &str, columns: bool) {
        let Some(t) = self.toks.get(j).filter(|_| j < end) else {
            return;
        };
        if t.t == T::Word && (self.txt(j).starts_with('@') || self.kw_in(j, RESERVED)) {
            return;
        }
        let Some((parts, k)) = self.name_chain(j, end) else {
            return;
        };
        // A name followed by `(` is a table-valued function, unless it is an `INSERT` target's
        // column list or a deprecated `(NOLOCK)` hint.
        if self.punct(k, b'(') && !columns {
            let hint = self.kw_in(k + 1, TABLE_HINTS) && self.punct(k + 2, b')');
            if !hint {
                return;
            }
        }
        let name = Self::object_name(&parts);
        if name.starts_with('#') || name.starts_with('@') || name.is_empty() {
            return;
        }
        self.out.edge(
            "QUERIES",
            current,
            &name,
            props(&[("confidence_band", json!("high"))]),
        );
    }

    /// An `EXEC` statement calling a named procedure (not `EXECUTE AS`, a string, a procedure
    /// variable or the source of an `INSERT ... EXEC`).
    fn exec(&mut self, i: usize, end: usize, current: &str) {
        let mut j = i + 1;
        if self.kw(j, "AS") || self.punct(j, b'(') {
            return;
        }
        if self.txt(j).starts_with('@') {
            if self.punct(j + 1, b'=') {
                j += 2;
            } else {
                return;
            }
        }
        if self.txt(j).starts_with('@') || self.punct(j, b'(') {
            return;
        }
        let Some((parts, _)) = self.name_chain(j, end) else {
            return;
        };
        if self.insert_source(i) {
            return;
        }
        let name = Self::object_name(&parts);
        if name.is_empty() {
            return;
        }
        self.out.edge(
            "CALLS",
            current,
            &name,
            props(&[("confidence_band", json!("high"))]),
        );
    }

    /// Whether the `EXEC` at `i` follows `INSERT [INTO] table [(columns)]`.
    fn insert_source(&self, i: usize) -> bool {
        let Some(mut k) = i.checked_sub(1) else {
            return false;
        };
        if self.punct(k, b')') {
            let mut depth = 0usize;
            loop {
                if self.punct(k, b')') {
                    depth += 1;
                } else if self.punct(k, b'(') {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                let Some(p) = k.checked_sub(1) else {
                    return false;
                };
                k = p;
            }
            let Some(p) = k.checked_sub(1) else {
                return false;
            };
            k = p;
        }
        let mut chain = 0;
        while matches!(
            self.toks.get(k).map(|t| t.t),
            Some(T::Word | T::Quoted | T::Punct(b'.'))
        ) && !self.kw_in(k, &["INTO", "INSERT"])
        {
            chain += 1;
            let Some(p) = k.checked_sub(1) else {
                return false;
            };
            k = p;
        }
        if chain == 0 {
            return false;
        }
        if self.kw(k, "INTO") {
            let Some(p) = k.checked_sub(1) else {
                return false;
            };
            k = p;
        }
        self.kw(k, "INSERT")
    }
}
