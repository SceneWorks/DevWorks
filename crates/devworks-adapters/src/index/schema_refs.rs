//! Code-to-schema evidence: SQL statements written whole in string literals, and ORM mappings
//! that name their table or column explicitly. Each table a file names is one `DatabaseTable` node
//! of that file (qualified name `db-table:<name>`, lowercased, `schema.table` when qualified), and
//! each use is an edge from the smallest extracted definition containing it: `USES_TABLE`, or
//! `USES_COLUMN` with the column. Every edge carries its evidence (`extractor`, `line`, a bounded
//! `snippet`, and for SQL the `statement`).
//!
//! Not covered, because the table name is not written where the code uses it: names an ORM derives
//! by convention (ActiveRecord pluralization, Django `app_model`, GORM snake-case, EF `DbSet`
//! names, Sequelize model names), SQL assembled by concatenation or interpolation, Swift and Scala
//! (no dominant SQL ORM), and Prisma schema files (not a parsed language).
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractedEdge, ExtractedNode, FileExtract, Resolution, Span,
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;
use tree_sitter::Node;

/// The languages whose literals and ORM mappings are read.
pub(crate) fn covers(language: &str) -> bool {
    matches!(
        language,
        "csharp"
            | "python"
            | "go"
            | "java"
            | "kotlin"
            | "ruby"
            | "rust"
            | "php"
            | "javascript"
            | "typescript"
    )
}

const SNIPPET_CHARS: usize = 256;
const MAX_DEPTH: usize = super::treesitter::MAX_DEPTH;

/// One use of a table found in the source.
struct Use {
    table: String,
    column: Option<String>,
    at: usize,
    /// The definition the use was attributed to, when not the smallest one containing `at`.
    owner: Option<usize>,
    extractor: &'static str,
    statement: Option<&'static str>,
    snippet: String,
    confidence: f32,
}

/// A table an ORM maps over a byte range of the source (a class, a call, a macro block): the
/// columns mapped inside it belong to that table.
struct Mapping {
    table: String,
    range: (usize, usize),
}

/// Adds the file's schema evidence to `out`.
pub(crate) fn extract(language: &str, source: &[u8], root: Node<'_>, out: &mut FileExtract) {
    let Ok(text) = std::str::from_utf8(source) else {
        return;
    };
    let mut uses = Vec::new();
    literals(root, source, &mut uses);
    orm(language, text, root, out, &mut uses);
    record(source, uses, out);
}

// -- SQL in string literals ---------------------------------------------------------------------

/// Every outermost string literal without interpolation that holds a whole SQL statement.
fn literals(root: Node<'_>, source: &[u8], uses: &mut Vec<Use>) {
    let mut stack = vec![(root, 0usize)];
    while let Some((n, depth)) = stack.pop() {
        if depth >= MAX_DEPTH {
            continue;
        }
        if n.kind().contains("string") && !n.kind().contains("content") {
            if !interpolated(n)
                && let Ok(raw) = n.utf8_text(source)
                && let Some(sql) = unquote(raw)
            {
                for found in statement(sql) {
                    uses.push(Use {
                        snippet: snippet(sql),
                        at: n.start_byte(),
                        extractor: "sql-literal",
                        confidence: 0.8,
                        ..found
                    });
                }
            }
            continue;
        }
        let mut cursor = n.walk();
        for c in n.named_children(&mut cursor) {
            stack.push((c, depth + 1));
        }
    }
}

/// Whether a literal splices in values (`${…}`, `#{…}`, `{…}` of an f-string, `$var`, and the
/// `interpolated_expression`/`interpolated_identifier` nodes of Kotlin-style templates).
fn interpolated(n: Node<'_>) -> bool {
    let mut stack = vec![n];
    while let Some(n) = stack.pop() {
        let kind = n.kind();
        if kind.contains("interpolat")
            || kind.contains("substitution")
            || kind == "variable_name"
            || kind == "simple_variable"
        {
            return true;
        }
        let mut cursor = n.walk();
        stack.extend(n.named_children(&mut cursor));
    }
    false
}

/// The text inside a literal's quotes, or `None` for an f-string or a literal with no quotes.
fn unquote(raw: &str) -> Option<&str> {
    let open = raw.find(['"', '\'', '`'])?;
    let prefix = &raw[..open];
    if prefix.contains(['f', 'F', '$']) {
        return None;
    }
    let body = raw[open..].trim_start_matches(['"', '\'', '`']);
    let body = body.trim_end_matches('#');
    Some(body.trim_end_matches(['"', '\'', '`']))
}

fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(SNIPPET_CHARS).collect()
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    /// A quoted identifier (`"x"`, `` `x` ``, `[x]`), lowercased like a word.
    Quoted(String),
    Punct(char),
    Other,
}

fn tokens(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '#' && !matches!(chars.get(i + 1), Some('>' | '-')) {
            // A MySQL comment (not Postgres's `#>`, `#>>` and `#-` operators).
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$')) {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else if matches!(c, '"' | '`' | '[') {
            let close = if c == '[' { ']' } else { c };
            let start = i + 1;
            i = start;
            while i < chars.len() && chars[i] != close {
                i += 1;
            }
            out.push(Tok::Quoted(
                chars[start..i.min(chars.len())].iter().collect(),
            ));
            i += 1;
        } else if c == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                i += 1;
            }
            i += 1;
            out.push(Tok::Other);
        } else if matches!(c, '.' | ',' | '(' | ')' | ';' | '=') {
            out.push(Tok::Punct(c));
            i += 1;
        } else {
            out.push(Tok::Other);
            i += 1;
        }
    }
    out
}

fn is_kw(t: Option<&Tok>, kw: &str) -> bool {
    matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
}

/// Whether the text reads as SQL rather than prose: it has SQL punctuation or an operator, or its
/// statement keywords are written in capitals ("Select a file from the list" has neither).
fn sql_shaped(sql: &str) -> bool {
    if sql.contains(['*', ',', '(', '=', '?', ';', '@', '$', ':', '<', '>', '.']) {
        return true;
    }
    sql.split_whitespace()
        .filter(|w| {
            [
                "select", "from", "insert", "into", "update", "set", "delete", "where", "join",
            ]
            .iter()
            .any(|k| k.eq_ignore_ascii_case(w))
        })
        .all(|w| w.chars().all(|c| c.is_ascii_uppercase()))
}

/// Words that end a table list or never name a table.
const STOP: &[&str] = &[
    "where",
    "on",
    "using",
    "set",
    "values",
    "select",
    "group",
    "order",
    "having",
    "limit",
    "union",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "natural",
    "as",
    "returning",
    "output",
    "default",
    "when",
    "then",
    "offset",
    "fetch",
    "for",
    "lateral",
    "window",
    "except",
    "intersect",
    "with",
    "and",
    "or",
    "not",
    "into",
    "from",
    "update",
    "delete",
    "insert",
];

fn stop_word(w: &str) -> bool {
    STOP.iter().any(|s| s.eq_ignore_ascii_case(w))
}

/// A dotted name starting at `i` (`schema.table`, quoted parts allowed), lowercased, and the
/// position after it.
fn name_at(toks: &[Tok], mut i: usize) -> Option<(String, usize)> {
    let mut parts = Vec::new();
    loop {
        let part = match toks.get(i)? {
            Tok::Word(w) if parts.is_empty() && stop_word(w) => return None,
            Tok::Word(w) | Tok::Quoted(w) => w.to_lowercase(),
            _ => return None,
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        i += 1;
        if toks.get(i) == Some(&Tok::Punct('.')) {
            i += 1;
        } else {
            break;
        }
    }
    // A catalog-qualified name keeps its schema and table.
    let keep = parts.len().saturating_sub(2);
    Some((parts[keep..].join("."), i))
}

/// The tables (with their columns where the statement names them unambiguously) of one SQL
/// statement, or none when the text is not one: a statement keyword first, then the clause that
/// keyword requires (`SELECT … FROM`, `INSERT INTO … (`/`VALUES`/`SELECT`, `UPDATE … SET`,
/// `DELETE FROM`, `MERGE INTO`), so prose such as "update your profile" is not read as SQL.
fn statement(sql: &str) -> Vec<Use> {
    if !sql_shaped(sql) {
        return Vec::new();
    }
    let toks = tokens(sql);
    let first = toks.iter().position(|t| *t != Tok::Punct('('));
    let Some(Tok::Word(lead)) = first.and_then(|i| toks.get(i)) else {
        return Vec::new();
    };
    let lead = lead.to_lowercase();
    let kind: &'static str = match lead.as_str() {
        "select" => "select",
        "insert" => "insert",
        "update" => "update",
        "delete" => "delete",
        "with" => "with",
        "merge" => "merge",
        _ => return Vec::new(),
    };
    // Common table expressions are not tables.
    let mut ctes = Vec::new();
    let mut first_cte = None;
    if kind == "with" {
        for (i, t) in toks.iter().enumerate() {
            if let Tok::Word(w) = t
                && is_kw(toks.get(i + 1), "as")
                && toks.get(i + 2) == Some(&Tok::Punct('('))
            {
                first_cte.get_or_insert(i);
                ctes.push(w.to_lowercase());
            }
        }
        // A `WITH` that is SQL defines `name AS (…)` and then runs a statement over it.
        let runs = first_cte.is_some_and(|f| {
            toks[f..].iter().any(|t| {
                ["select", "insert", "update", "delete"]
                    .iter()
                    .any(|k| is_kw(Some(t), k))
            })
        });
        if !runs {
            return Vec::new();
        }
    }
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let add = |tables: &mut Vec<(String, Vec<String>)>, name: &str| {
        if ctes.iter().any(|c| c == name) {
            return None;
        }
        Some(match tables.iter().position(|(t, _)| t == name) {
            Some(at) => at,
            None => {
                tables.push((name.to_owned(), Vec::new()));
                tables.len() - 1
            }
        })
    };
    let mut valid = false;
    let mut i = 0;
    while i < toks.len() {
        let here = toks.get(i);
        let from = is_kw(here, "from") || is_kw(here, "join");
        let into = (is_kw(here, "into")
            && (is_kw(i.checked_sub(1).and_then(|p| toks.get(p)), "insert")
                || is_kw(i.checked_sub(1).and_then(|p| toks.get(p)), "merge")))
            || (is_kw(here, "update") && i == first.unwrap_or(0));
        if !(from || into) {
            i += 1;
            continue;
        }
        // `SELECT … FROM` needs a select list; `DELETE FROM` is its own clause.
        if is_kw(here, "from") && kind == "select" && first.is_some_and(|f| i == f + 1) {
            return Vec::new();
        }
        let mut j = i + 1;
        loop {
            if toks.get(j) == Some(&Tok::Punct('(')) {
                break;
            }
            let Some((name, after)) = name_at(&toks, j) else {
                break;
            };
            // A table function (`generate_series(1, 3) g`) is not a table: read past it.
            if from && toks.get(after) == Some(&Tok::Punct('(')) {
                let mut depth = 0;
                let mut k = after;
                while let Some(t) = toks.get(k) {
                    match t {
                        Tok::Punct('(') => depth += 1,
                        Tok::Punct(')') => {
                            depth -= 1;
                            if depth == 0 {
                                k += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                if is_kw(toks.get(k), "as") {
                    k += 1;
                }
                if matches!(toks.get(k), Some(Tok::Word(a)) if !stop_word(a)) {
                    k += 1;
                }
                if toks.get(k) == Some(&Tok::Punct(',')) {
                    j = k + 1;
                    continue;
                }
                break;
            }
            // A common table expression is read past, not recorded.
            let at = add(&mut tables, &name);
            let mut k = after;
            if is_kw(toks.get(k), "as") {
                k += 1;
            }
            if let Some(Tok::Word(alias)) = toks.get(k)
                && !stop_word(alias)
            {
                if at.is_some() {
                    aliases.insert(alias.to_lowercase(), name.clone());
                }
                k += 1;
            }
            if at.is_some() {
                aliases.insert(
                    name.rsplit('.').next().unwrap_or(&name).to_owned(),
                    name.clone(),
                );
            }
            // What the clause must be followed by to be SQL rather than prose.
            let follow = toks.get(k);
            if is_kw(here, "into") {
                valid |= follow == Some(&Tok::Punct('('))
                    || is_kw(follow, "values")
                    || is_kw(follow, "select")
                    || is_kw(follow, "using")
                    || is_kw(follow, "default");
                if let Some(at) = at
                    && follow == Some(&Tok::Punct('('))
                    && kind == "insert"
                {
                    let mut c = k + 1;
                    while let Some(t) = toks.get(c) {
                        match t {
                            Tok::Word(w) | Tok::Quoted(w) => tables[at].1.push(w.to_lowercase()),
                            Tok::Punct(',') => {}
                            _ => break,
                        }
                        c += 1;
                    }
                }
            } else if is_kw(here, "update") {
                if let Some(at) = at
                    && is_kw(follow, "set")
                {
                    valid = true;
                    let mut c = k + 1;
                    while let Some(Tok::Word(w) | Tok::Quoted(w)) = toks.get(c) {
                        if toks.get(c + 1) != Some(&Tok::Punct('=')) {
                            break;
                        }
                        tables[at].1.push(w.to_lowercase());
                        // Skip the value to the next top-level comma.
                        let mut depth = 0;
                        c += 2;
                        while let Some(t) = toks.get(c) {
                            match t {
                                Tok::Punct('(') => depth += 1,
                                Tok::Punct(')') => depth -= 1,
                                Tok::Punct(',') if depth == 0 => break,
                                Tok::Word(w) if depth == 0 && stop_word(w) => break,
                                _ => {}
                            }
                            c += 1;
                        }
                        if toks.get(c) != Some(&Tok::Punct(',')) {
                            break;
                        }
                        c += 1;
                    }
                }
            } else {
                // A comma continues the table list, so a name or a subquery must follow it
                // ("Select items from the list, then …" is prose).
                valid |= follow.is_none()
                    || matches!(follow, Some(Tok::Punct(';' | ')')))
                    || (follow == Some(&Tok::Punct(','))
                        && (name_at(&toks, k + 1).is_some()
                            || toks.get(k + 1) == Some(&Tok::Punct('('))))
                    || matches!(follow, Some(Tok::Word(w)) if stop_word(w));
            }
            if toks.get(k) == Some(&Tok::Punct(',')) && is_kw(here, "from") {
                j = k + 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    if !valid || tables.is_empty() {
        return Vec::new();
    }
    // `alias.column` and `table.column` name a column of that table.
    for w in toks.windows(3) {
        if let [
            Tok::Word(a) | Tok::Quoted(a),
            Tok::Punct('.'),
            Tok::Word(c) | Tok::Quoted(c),
        ] = w
            && let Some(table) = aliases.get(&a.to_lowercase())
            && let Some(entry) = tables.iter_mut().find(|(t, _)| t == table)
            && !entry.1.contains(&c.to_lowercase())
        {
            entry.1.push(c.to_lowercase());
        }
    }
    let mut out = Vec::new();
    for (table, columns) in tables {
        let mut seen = Vec::new();
        for column in std::iter::once(None).chain(columns.into_iter().map(Some)) {
            if column
                .as_ref()
                .is_some_and(|c| seen.contains(c) || c == "*")
            {
                continue;
            }
            if let Some(c) = &column {
                seen.push(c.clone());
            }
            out.push(Use {
                table: table.clone(),
                column,
                at: 0,
                owner: None,
                extractor: "sql-literal",
                statement: Some(kind),
                snippet: String::new(),
                confidence: 0.8,
            });
        }
    }
    out
}

// -- ORM mappings -------------------------------------------------------------------------------

fn re(p: &str) -> Regex {
    Regex::new(p).expect("valid ORM pattern")
}

/// A table mapping: the extractor, its pattern, and the capture groups of the table and schema.
struct TablePattern {
    extractor: &'static str,
    languages: &'static [&'static str],
    regex: LazyLock<Regex>,
    /// What the columns mapped inside belong to: the enclosing definition, or the match's own
    /// parenthesized or braced extent.
    scope: Scope,
    /// An annotation, attribute or decorator: it belongs to the definition it precedes when no
    /// definition contains it (a TypeScript decorator or a Rust attribute lies before its item).
    precedes: bool,
}
struct ColumnPattern {
    extractor: &'static str,
    languages: &'static [&'static str],
    regex: LazyLock<Regex>,
}
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    /// The smallest extracted definition containing the match.
    Owner,
    /// That definition's container (Django's `class Meta` inside its model).
    OwnerParent,
    /// The call or macro block the match opens.
    Block,
}

const JVM: &[&str] = &["java", "kotlin"];
const JS: &[&str] = &["javascript", "typescript"];

static TABLES: [TablePattern; 11] = [
    TablePattern {
        extractor: "orm:jpa",
        languages: JVM,
        regex: LazyLock::new(|| {
            re(
                r#"@(?:(?:javax|jakarta)\.persistence\.)?Table\s*\((?:[^)]*?\bschema\s*=\s*"(?P<schema>[^"]+)")?[^)]*?\bname\s*=\s*"(?P<table>[^"]+)""#,
            )
        }),
        scope: Scope::Owner,
        precedes: true,
    },
    TablePattern {
        extractor: "orm:ef-core",
        languages: &["csharp"],
        regex: LazyLock::new(|| {
            re(
                r#"\[\s*Table\s*\(\s*"(?P<table>[^"]+)"(?:\s*,\s*Schema\s*=\s*"(?P<schema>[^"]+)")?"#,
            )
        }),
        scope: Scope::Owner,
        precedes: true,
    },
    TablePattern {
        extractor: "orm:ef-core",
        languages: &["csharp"],
        regex: LazyLock::new(|| {
            re(r#"\.ToTable\s*\(\s*"(?P<table>[^"]+)"(?:\s*,\s*"(?P<schema>[^"]+)")?"#)
        }),
        scope: Scope::Owner,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:sqlalchemy",
        languages: &["python"],
        regex: LazyLock::new(|| re(r#"__tablename__\s*=\s*["'](?P<table>[^"']+)["']"#)),
        scope: Scope::Owner,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:sqlalchemy",
        languages: &["python"],
        regex: LazyLock::new(|| {
            re(
                r#"\bTable\s*\(\s*["'](?P<table>[^"']+)["']\s*,\s*(?:\w+\.)*\w*meta\w*(?:[^)]*?\bschema\s*=\s*["'](?P<schema>[^"']+)["'])?"#,
            )
        }),
        scope: Scope::Block,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:django",
        languages: &["python"],
        regex: LazyLock::new(|| re(r#"\bdb_table\s*=\s*["'](?P<table>[^"']+)["']"#)),
        scope: Scope::OwnerParent,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:active-record",
        languages: &["ruby"],
        regex: LazyLock::new(|| re(r#"self\.table_name\s*=\s*["'](?P<table>[^"']+)["']"#)),
        scope: Scope::Owner,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:eloquent",
        languages: &["php"],
        regex: LazyLock::new(|| {
            re(r#"protected\s+(?:string\s+)?\$table\s*=\s*['"](?P<table>[^'"]+)['"]"#)
        }),
        scope: Scope::Owner,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:doctrine",
        languages: &["php"],
        regex: LazyLock::new(|| {
            re(
                r#"(?:#\[|@)ORM\\Table\s*\([^)]*?\bname\s*[:=]\s*['"](?P<table>[^'"]+)['"](?:[^)]*?\bschema\s*[:=]\s*['"](?P<schema>[^'"]+)['"])?"#,
            )
        }),
        scope: Scope::Owner,
        precedes: true,
    },
    TablePattern {
        extractor: "orm:typeorm",
        languages: JS,
        regex: LazyLock::new(|| {
            re(
                r#"@Entity\s*\(\s*(?:["'](?P<table>[^"']+)["']|\{[^}]*?\bname\s*:\s*["'](?P<named>[^"']+)["'](?:[^}]*?\bschema\s*:\s*["'](?P<schema>[^"']+)["'])?)"#,
            )
        }),
        scope: Scope::Owner,
        precedes: true,
    },
    TablePattern {
        extractor: "orm:sequelize",
        languages: JS,
        regex: LazyLock::new(|| re(r#"\btableName\s*:\s*["'](?P<table>[^"']+)["']"#)),
        scope: Scope::Owner,
        precedes: false,
    },
];
static MORE_TABLES: [TablePattern; 3] = [
    TablePattern {
        extractor: "orm:gorm",
        languages: &["go"],
        regex: LazyLock::new(|| {
            re(
                r#"func\s*\(\s*(?:\w+\s+)?\*?\s*(?P<type>\w+)\s*\)\s*TableName\s*\(\s*\)\s*string\s*\{\s*return\s+"(?P<table>[^"]+)""#,
            )
        }),
        scope: Scope::Owner,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:diesel",
        languages: &["rust"],
        regex: LazyLock::new(|| {
            re(r"\btable!\s*\{\s*(?:(?P<schema>\w+)\s*\.\s*)?(?P<table>\w+)\s*(?:\([^)]*\)\s*)?\{")
        }),
        scope: Scope::Block,
        precedes: false,
    },
    TablePattern {
        extractor: "orm:diesel",
        languages: &["rust"],
        regex: LazyLock::new(|| {
            re(
                r#"#\[\s*(?:diesel\s*\(\s*table_name\s*=\s*(?:[\w:]+::)?(?P<table>\w+)|table_name\s*=\s*"(?P<quoted>[^"]+)"|sea_orm\s*\((?:[^)]*?\bschema_name\s*=\s*"(?P<schema>[^"]+)")?[^)]*?\btable_name\s*=\s*"(?P<sea>[^"]+)")"#,
            )
        }),
        scope: Scope::Owner,
        precedes: true,
    },
];

static COLUMNS: [ColumnPattern; 8] = [
    ColumnPattern {
        extractor: "orm:jpa",
        languages: JVM,
        regex: LazyLock::new(|| {
            re(
                r#"@(?:(?:javax|jakarta)\.persistence\.)?Column\s*\([^)]*?\bname\s*=\s*"(?P<column>[^"]+)""#,
            )
        }),
    },
    ColumnPattern {
        extractor: "orm:ef-core",
        languages: &["csharp"],
        regex: LazyLock::new(|| re(r#"\[\s*Column\s*\(\s*"(?P<column>[^"]+)""#)),
    },
    ColumnPattern {
        extractor: "orm:sqlalchemy",
        languages: &["python"],
        regex: LazyLock::new(|| {
            re(r#"\b(?:Column|mapped_column)\s*\(\s*["'](?P<column>[^"']+)["']"#)
        }),
    },
    ColumnPattern {
        extractor: "orm:django",
        languages: &["python"],
        regex: LazyLock::new(|| re(r#"\bdb_column\s*=\s*["'](?P<column>[^"']+)["']"#)),
    },
    ColumnPattern {
        extractor: "orm:doctrine",
        languages: &["php"],
        regex: LazyLock::new(|| {
            re(r#"(?:#\[|@)ORM\\Column\s*\([^)]*?\bname\s*[:=]\s*['"](?P<column>[^'"]+)['"]"#)
        }),
    },
    ColumnPattern {
        extractor: "orm:typeorm",
        languages: JS,
        regex: LazyLock::new(|| {
            re(
                r#"@(?:Primary(?:Generated)?)?Column\s*\([^)]*?\bname\s*:\s*["'](?P<column>[^"']+)["']"#,
            )
        }),
    },
    ColumnPattern {
        extractor: "orm:gorm",
        languages: &["go"],
        regex: LazyLock::new(|| re(r#"gorm:"[^"]*?\bcolumn:(?P<column>[A-Za-z0-9_]+)"#)),
    },
    ColumnPattern {
        extractor: "orm:diesel",
        languages: &["rust"],
        regex: LazyLock::new(|| re(r"(?m)^\s*(?P<column>\w+)\s*->\s*[\w<>]+\s*,?\s*$")),
    },
];

/// Whether the byte at `at` lies in a comment (a commented-out mapping is not one). Doctrine
/// annotations live in docblocks, so they are read there.
fn in_comment(root: Node<'_>, at: usize) -> bool {
    root.descendant_for_byte_range(at, at)
        .is_some_and(|n| n.kind().contains("comment"))
}

/// The smallest extracted definition (not the file, unless nothing else) whose span holds `at`.
fn owner(out: &FileExtract, at: usize) -> usize {
    let mut best = 0;
    let mut width = usize::MAX;
    for (i, n) in out.nodes.iter().enumerate().skip(1) {
        let (start, end) = (n.span.start_byte as usize, n.span.end_byte as usize);
        if n.kind != "DatabaseTable" && start <= at && at < end && end - start < width {
            best = i;
            width = end - start;
        }
    }
    best
}

/// The definition an annotation ending at `end` precedes: the first one starting after it.
fn following(out: &FileExtract, end: usize) -> Option<usize> {
    out.nodes
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, n)| n.kind != "DatabaseTable" && n.span.start_byte as usize >= end)
        .min_by_key(|(_, n)| n.span.start_byte)
        .map(|(i, _)| i)
}

/// The smallest definition whose span strictly contains definition `inner`'s (the file at most).
fn container(out: &FileExtract, inner: usize) -> usize {
    let (start, end) = (
        out.nodes[inner].span.start_byte,
        out.nodes[inner].span.end_byte,
    );
    out.nodes
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(i, n)| {
            *i != inner
                && n.kind != "DatabaseTable"
                && n.span.start_byte <= start
                && end <= n.span.end_byte
                && n.span.end_byte - n.span.start_byte > end - start
        })
        .min_by_key(|(_, n)| n.span.end_byte - n.span.start_byte)
        .map_or(0, |(i, _)| i)
}

/// The extent of the bracketed block that starts at or after `at` (its `(` or `{`).
fn block(text: &str, at: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let Some(open) = bytes[at..].iter().position(|b| matches!(b, b'(' | b'{')) else {
        return (at, at);
    };
    let mut depth = 0i32;
    for (i, b) in bytes.iter().enumerate().skip(at + open) {
        match b {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return (at, i + 1);
                }
            }
            _ => {}
        }
    }
    (at, bytes.len())
}

fn qualified(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", s.to_lowercase(), table.to_lowercase()),
        None => table.to_lowercase(),
    }
}

fn orm(language: &str, text: &str, root: Node<'_>, out: &FileExtract, uses: &mut Vec<Use>) {
    let mut mappings: Vec<Mapping> = Vec::new();
    let mut types: BTreeMap<String, String> = BTreeMap::new();
    for pattern in TABLES.iter().chain(MORE_TABLES.iter()) {
        if !pattern.languages.contains(&language) {
            continue;
        }
        for m in pattern.regex.captures_iter(text) {
            let whole = m.get(0).expect("match");
            if pattern.extractor != "orm:doctrine" && in_comment(root, whole.start()) {
                continue;
            }
            let Some(table) = ["table", "named", "quoted", "sea"]
                .iter()
                .find_map(|g| m.name(g))
                .map(|t| t.as_str())
            else {
                continue;
            };
            let table = qualified(m.name("schema").map(|s| s.as_str()), table);
            let at = whole.start();
            let mut owner_at = owner(out, at);
            if owner_at == 0
                && pattern.precedes
                && let Some(next) = following(out, whole.end())
            {
                owner_at = next;
            }
            if pattern.scope == Scope::OwnerParent && owner_at != 0 {
                owner_at = container(out, owner_at);
            }
            let range = match pattern.scope {
                Scope::Block => block(text, at),
                _ => {
                    let span = &out.nodes[owner_at].span;
                    ((span.start_byte as usize).min(at), span.end_byte as usize)
                }
            };
            if let Some(ty) = m.name("type") {
                types.insert(ty.as_str().to_owned(), table.clone());
            }
            mappings.push(Mapping {
                table: table.clone(),
                range,
            });
            uses.push(Use {
                table,
                column: None,
                at,
                owner: Some(owner_at),
                extractor: pattern.extractor,
                statement: None,
                snippet: snippet(whole.as_str()),
                confidence: 0.95,
            });
        }
    }
    // A GORM type maps its table through `TableName`; the fields of its struct declaration are
    // that table's columns.
    for (ty, table) in &types {
        let declaration = re(&format!(r"\btype\s+{}\s+struct\s*\{{", regex::escape(ty)));
        for m in declaration.find_iter(text) {
            mappings.push(Mapping {
                table: table.clone(),
                range: block(text, m.start()),
            });
        }
    }
    for pattern in &COLUMNS {
        if !pattern.languages.contains(&language) {
            continue;
        }
        for m in pattern.regex.captures_iter(text) {
            let whole = m.get(0).expect("match");
            let at = m.name("column").map_or(whole.start(), |c| c.start());
            if pattern.extractor != "orm:doctrine" && in_comment(root, at) {
                continue;
            }
            // The innermost mapping around the column names its table; a column outside every
            // mapping names no table and is not a use.
            let Some(mapping) = mappings
                .iter()
                .filter(|map| map.range.0 <= at && at < map.range.1)
                .min_by_key(|map| map.range.1 - map.range.0)
            else {
                continue;
            };
            uses.push(Use {
                table: mapping.table.clone(),
                column: Some(m["column"].to_lowercase()),
                at,
                owner: None,
                extractor: pattern.extractor,
                statement: None,
                snippet: snippet(whole.as_str()),
                confidence: 0.9,
            });
        }
    }
}

// -- Output -------------------------------------------------------------------------------------

fn line_of(source: &[u8], at: usize) -> u32 {
    source[..at.min(source.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
        + 1
}

fn record(source: &[u8], uses: Vec<Use>, out: &mut FileExtract) {
    let mut tables: BTreeMap<String, usize> = BTreeMap::new();
    for found in uses {
        if out.nodes.len() >= super::treesitter::MAX_NODES
            || out.edges.len() >= super::treesitter::MAX_EDGES
        {
            out.incomplete
                .push("schema-references: node or edge limit".into());
            return;
        }
        let line = line_of(source, found.at);
        let source_node = found.owner.unwrap_or_else(|| owner(out, found.at));
        let target = match tables.get(&found.table) {
            Some(t) => *t,
            None => {
                let at = found.at as u32;
                out.nodes.push(ExtractedNode {
                    kind: "DatabaseTable".into(),
                    name: found.table.clone(),
                    qualified_name: format!("db-table:{}", found.table),
                    span: Span {
                        start_line: line,
                        start_column: 0,
                        end_line: line,
                        end_column: 0,
                        start_byte: at,
                        end_byte: at,
                    },
                    parent: Some(0),
                    properties: Map::from_iter([("table".into(), json!(found.table))]),
                });
                tables.insert(found.table.clone(), out.nodes.len() - 1);
                out.nodes.len() - 1
            }
        };
        let mut properties = Map::new();
        properties.insert("extractor".into(), json!(found.extractor));
        properties.insert("line".into(), json!(line));
        properties.insert("snippet".into(), json!(found.snippet));
        properties.insert("table".into(), json!(found.table));
        if let Some(statement) = found.statement {
            properties.insert("statement".into(), json!(statement));
        }
        let kind = match &found.column {
            Some(column) => {
                properties.insert("column".into(), Value::String(column.clone()));
                "USES_COLUMN"
            }
            None => "USES_TABLE",
        };
        out.edges.push(ExtractedEdge {
            kind: kind.into(),
            source: source_node,
            target: EdgeTarget::Local(target),
            resolution: Resolution::Resolved,
            confidence: found.confidence,
            properties,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(sql: &str) -> Vec<(String, Option<String>)> {
        statement(sql)
            .into_iter()
            .map(|u| (u.table, u.column))
            .collect()
    }
    fn t(name: &str) -> (String, Option<String>) {
        (name.into(), None)
    }
    fn c(name: &str, column: &str) -> (String, Option<String>) {
        (name.into(), Some(column.into()))
    }

    #[test]
    fn statements_name_their_tables_and_unambiguous_columns() {
        assert_eq!(
            tables(
                "SELECT o.id, o.total FROM orders o JOIN customers c ON c.id = \
                    o.customer_id WHERE o.id = ?"
            ),
            [
                t("orders"),
                c("orders", "id"),
                c("orders", "total"),
                c("orders", "customer_id"),
                t("customers"),
                c("customers", "id")
            ]
        );
        assert_eq!(
            tables("insert into public.\"Orders\" (id, total) values ($1, $2)"),
            [
                t("public.orders"),
                c("public.orders", "id"),
                c("public.orders", "total")
            ]
        );
        assert_eq!(
            tables("UPDATE accounts SET balance = balance - ?, updated_at = now() WHERE id = ?"),
            [
                t("accounts"),
                c("accounts", "balance"),
                c("accounts", "updated_at")
            ]
        );
        assert_eq!(
            tables("DELETE FROM sessions WHERE expires < ?"),
            [t("sessions")]
        );
        assert_eq!(
            tables("WITH recent AS (SELECT * FROM events) SELECT * FROM recent, users"),
            [t("events"), t("users")]
        );
        assert_eq!(
            tables("SELECT a FROM [dbo].[Items], `shop`.`carts`"),
            [t("dbo.items"), t("shop.carts")]
        );
        assert_eq!(
            tables("MERGE INTO stock USING incoming ON stock.sku = incoming.sku"),
            [t("stock"), c("stock", "sku")]
        );
    }

    #[test]
    fn prose_and_fragments_are_not_sql() {
        for text in [
            "Select a file from the list",
            "Please update your profile",
            "Delete from cart?",
            "insert into the box",
            "SELECT FROM",
            "selected items",
            "SELECT 1",
            "from users",
            "select name from users",
            "With a mouse, select from the menu",
            "Select items from the list, then press Enter",
            "SELECT * FROM generate_series(1, 3) g",
        ] {
            assert_eq!(tables(text), [], "{text}");
        }
    }

    /// Each use the parser records in `source`: the edge kind, the using definition's name, the
    /// table, the column, and the extractor.
    fn uses(
        language: &str,
        path: &str,
        source: &str,
    ) -> Vec<(String, String, String, String, String)> {
        use devworks_core::ports::indexing::StructuralParser;
        let x = super::super::treesitter::TreeSitterParser::new()
            .extract(language, path, "repo", source.as_bytes(), &|| false, 10_000)
            .unwrap();
        x.edges
            .iter()
            .filter(|e| e.kind.starts_with("USES_"))
            .map(|e| {
                let EdgeTarget::Local(target) = e.target else {
                    panic!("a schema use targets its file's table node")
                };
                assert_eq!(x.nodes[target].kind, "DatabaseTable");
                let p = |k: &str| {
                    e.properties
                        .get(k)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                assert!(e.properties["line"].as_u64().is_some_and(|l| l > 0));
                assert!(!p("snippet").is_empty());
                (
                    e.kind.clone(),
                    x.nodes[e.source].name.clone(),
                    x.nodes[target].name.clone(),
                    p("column"),
                    p("extractor"),
                )
            })
            .collect()
    }
    fn has(
        found: &[(String, String, String, String, String)],
        kind: &str,
        owner: &str,
        table: &str,
        column: &str,
        extractor: &str,
    ) -> bool {
        found.iter().any(|f| {
            (
                f.0.as_str(),
                f.1.as_str(),
                f.2.as_str(),
                f.3.as_str(),
                f.4.as_str(),
            ) == (kind, owner, table, column, extractor)
        })
    }
    fn tables_of(
        found: &[(String, String, String, String, String)],
    ) -> std::collections::BTreeSet<String> {
        found.iter().map(|f| f.2.clone()).collect()
    }

    #[test]
    fn jpa_and_ef_core_mappings_and_sql_literals_link_their_tables() {
        let java = uses(
            "java",
            "Order.java",
            "import jakarta.persistence.*;\n@Entity\n@Table(schema = \"sales\", name \
                = \"orders\")\nclass Order {\n  @Column(name = \"total_cents\")\n  long \
                total;\n  // @Table(name = \"ghost\")\n  void load() { \
                jdbc.query(\"SELECT id FROM order_lines WHERE order_id = ?\"); }\n}\n",
        );
        assert!(
            has(&java, "USES_TABLE", "Order", "sales.orders", "", "orm:jpa"),
            "{java:?}"
        );
        assert!(
            has(
                &java,
                "USES_COLUMN",
                "Order",
                "sales.orders",
                "total_cents",
                "orm:jpa"
            ),
            "{java:?}"
        );
        assert!(
            has(
                &java,
                "USES_TABLE",
                "load",
                "order_lines",
                "",
                "sql-literal"
            ),
            "{java:?}"
        );
        assert!(!tables_of(&java).contains("ghost"), "{java:?}");
        let cs = uses(
            "csharp",
            "Invoice.cs",
            "[Table(\"Invoices\", Schema = \"billing\")]\npublic class Invoice {\n  \
                [Column(\"due_on\")] public DateTime Due { get; set; }\n}\nclass Db \
                {\n  void OnModelCreating(ModelBuilder b) { \
                b.Entity<Line>().ToTable(\"invoice_lines\"); }\n  void Find(string t) { \
                var a = \"SELECT * FROM invoices WHERE id = @id\"; var b = $\"SELECT * \
                FROM {t}\"; }\n}\n",
        );
        assert!(
            has(
                &cs,
                "USES_TABLE",
                "Invoice",
                "billing.invoices",
                "",
                "orm:ef-core"
            ),
            "{cs:?}"
        );
        assert!(
            has(
                &cs,
                "USES_COLUMN",
                "Due",
                "billing.invoices",
                "due_on",
                "orm:ef-core"
            ),
            "{cs:?}"
        );
        assert!(
            has(
                &cs,
                "USES_TABLE",
                "OnModelCreating",
                "invoice_lines",
                "",
                "orm:ef-core"
            ),
            "{cs:?}"
        );
        assert!(
            has(&cs, "USES_TABLE", "Find", "invoices", "", "sql-literal"),
            "{cs:?}"
        );
        assert_eq!(
            tables_of(&cs).len(),
            3,
            "the interpolated statement names no table: {cs:?}"
        );
    }

    #[test]
    fn comments_are_skipped_and_table_functions_are_not_tables() {
        assert_eq!(
            tables("SELECT id /* FROM ghost */ FROM users # FROM other\n WHERE id = ?"),
            [t("users")]
        );
        assert_eq!(
            tables("SELECT * FROM generate_series(1, 3) g, users u WHERE u.id = g"),
            [t("users"), c("users", "id")]
        );
        assert_eq!(
            tables("WITH recent AS (SELECT * FROM events) SELECT * FROM recent"),
            [t("events")]
        );
    }

    #[test]
    fn python_ruby_and_php_mappings_name_only_explicit_tables() {
        let py = uses(
            "python",
            "models.py",
            "class User(Base):\n    __tablename__ = \"users\"\n    name = \
                Column(\"user_name\", String)\n\nroles = Table(\"roles\", metadata, \
                Column(\"role_id\", Integer))\n\nteams = Table(\"teams\", \
                Base.metadata, Column(\"team_id\", Integer))\nlogs = \
                db.Table(\"audit_logs\", db.metadata, Column(\"log_id\", \
                Integer))\n\nclass Book(models.Model):\n    title = \
                models.CharField(db_column=\"book_title\")\n    class Meta:\n        \
                db_table = \"library_books\"\n\nclass Shelf(models.Model):\n    \
                pass\n\ndef find(cur, t):\n    cur.execute(\"SELECT id FROM users WHERE \
                id = %s\")\n    cur.execute(f\"SELECT id FROM {t}\")\n",
        );
        assert!(
            has(&py, "USES_TABLE", "User", "users", "", "orm:sqlalchemy"),
            "{py:?}"
        );
        assert!(
            has(
                &py,
                "USES_COLUMN",
                "User",
                "users",
                "user_name",
                "orm:sqlalchemy"
            ),
            "{py:?}"
        );
        assert!(
            py.iter().any(|f| f.2 == "roles" && f.3 == "role_id"),
            "{py:?}"
        );
        for (table, column) in [("teams", "team_id"), ("audit_logs", "log_id")] {
            assert!(py.iter().any(|f| f.2 == table && f.3 == column), "{py:?}");
        }
        assert!(
            has(&py, "USES_TABLE", "Book", "library_books", "", "orm:django"),
            "{py:?}"
        );
        assert!(
            py.iter()
                .any(|f| f.2 == "library_books" && f.3 == "book_title"),
            "{py:?}"
        );
        assert!(
            has(&py, "USES_TABLE", "find", "users", "", "sql-literal"),
            "{py:?}"
        );
        assert!(
            !tables_of(&py).contains("shelf"),
            "a convention name is not linked: {py:?}"
        );
        let rb = uses(
            "ruby",
            "account.rb",
            "class Account < ApplicationRecord\n  self.table_name = \
                \"legacy_accounts\"\nend\nclass Person < ApplicationRecord\nend\n",
        );
        assert!(
            has(
                &rb,
                "USES_TABLE",
                "Account",
                "legacy_accounts",
                "",
                "orm:active-record"
            ),
            "{rb:?}"
        );
        assert_eq!(tables_of(&rb).len(), 1, "{rb:?}");
        let php = uses(
            "php",
            "Flight.php",
            "<?php\nclass Flight extends Model {\n  protected $table = 'flights';\n  \
                function late() { return DB::select('SELECT * FROM flights WHERE \
                delayed = ?'); }\n}\n#[ORM\\Entity]\n#[ORM\\Table(name: \
                'products')]\nclass Product {\n  #[ORM\\Column(name: 'sku_code')]\n  \
                private string $sku;\n}\n",
        );
        assert!(
            has(&php, "USES_TABLE", "Flight", "flights", "", "orm:eloquent"),
            "{php:?}"
        );
        assert!(
            has(&php, "USES_TABLE", "late", "flights", "", "sql-literal"),
            "{php:?}"
        );
        assert!(
            has(
                &php,
                "USES_TABLE",
                "Product",
                "products",
                "",
                "orm:doctrine"
            ),
            "{php:?}"
        );
        assert!(
            php.iter().any(|f| f.2 == "products" && f.3 == "sku_code"),
            "{php:?}"
        );
    }

    #[test]
    fn typescript_go_and_rust_mappings_and_literals_link_their_tables() {
        let ts = uses(
            "typescript",
            "photo.ts",
            "@Entity(\"photos\")\nexport class Photo {\n  @Column({ name: \
                \"file_name\" }) fileName: string;\n}\nCart.init({}, { sequelize, \
                tableName: \"carts\" });\nconst User = sequelize.define(\"User\", \
                {});\nfunction q(t: string) { db.query(`SELECT * FROM ${t}`); \
                db.query(`SELECT * FROM carts WHERE id = $1`); }\n",
        );
        assert!(
            has(&ts, "USES_TABLE", "Photo", "photos", "", "orm:typeorm"),
            "{ts:?}"
        );
        assert!(
            has(
                &ts,
                "USES_COLUMN",
                "Photo",
                "photos",
                "file_name",
                "orm:typeorm"
            ),
            "{ts:?}"
        );
        assert!(
            ts.iter().any(|f| f.2 == "carts" && f.4 == "orm:sequelize"),
            "{ts:?}"
        );
        assert!(
            has(&ts, "USES_TABLE", "q", "carts", "", "sql-literal"),
            "{ts:?}"
        );
        assert!(
            !tables_of(&ts).contains("user"),
            "a Sequelize model name is not a table: {ts:?}"
        );
        let go = uses(
            "go",
            "user.go",
            "package m\ntype User struct {\n  Name string \
                `gorm:\"column:full_name\"`\n}\nfunc (User) TableName() string { return \
                \"app_users\" }\nfunc Find(db *DB) { db.Raw(\"SELECT * FROM app_users \
                WHERE id = ?\") }\n",
        );
        assert!(
            has(&go, "USES_TABLE", "TableName", "app_users", "", "orm:gorm"),
            "{go:?}"
        );
        assert!(
            go.iter().any(|f| f.2 == "app_users" && f.3 == "full_name"),
            "{go:?}"
        );
        assert!(
            has(&go, "USES_TABLE", "Find", "app_users", "", "sql-literal"),
            "{go:?}"
        );
        let rs = uses(
            "rust",
            "schema.rs",
            "diesel::table! {\n    posts (id) {\n        id -> Int4,\n        title \
                -> Text,\n    }\n}\n#[derive(Queryable)]\n#[diesel(table_name = \
                crate::schema::posts)]\nstruct Post { id: i32 }\n#[sea_orm(table_name = \
                \"cakes\")]\npub struct Model { id: i32 }\nfn find() { \
                sqlx::query(\"SELECT id FROM posts WHERE id = $1\"); }\n",
        );
        assert!(
            rs.iter()
                .any(|f| f.2 == "posts" && f.4 == "orm:diesel" && f.3.is_empty()),
            "{rs:?}"
        );
        assert!(
            rs.iter().any(|f| f.2 == "posts" && f.3 == "title"),
            "{rs:?}"
        );
        assert!(rs.iter().any(|f| f.2 == "cakes"), "{rs:?}");
        assert!(
            has(&rs, "USES_TABLE", "find", "posts", "", "sql-literal"),
            "{rs:?}"
        );
    }

    #[test]
    fn literals_unquote_and_skip_interpolated_strings() {
        assert_eq!(unquote(r#""SELECT 1""#), Some("SELECT 1"));
        assert_eq!(unquote(r##"r#"SELECT 1"#"##), Some("SELECT 1"));
        assert_eq!(unquote("'''SELECT 1'''"), Some("SELECT 1"));
        assert_eq!(unquote(r#"@"SELECT 1""#), Some("SELECT 1"));
        assert_eq!(unquote(r#"f"SELECT {x}""#), None);
        assert_eq!(unquote(r#"$"SELECT {x}""#), None);
    }
}
