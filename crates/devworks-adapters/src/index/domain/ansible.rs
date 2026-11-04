//! Ansible: a port of the retained `AnsibleParser`. Files are classified as the retained parser
//! did (role directories by path, `requirements.yml`, playbooks and task lists by content); other
//! YAML yields nothing. The first YAML document is read into a representation model with the
//! retained YamlDotNet semantics (scalars as strings, aliases resolved, duplicate keys and syntax
//! errors fail the file) and walked for `Playbook`, `Role`, `AnsibleTask`, `AnsibleHandler` and
//! `AnsibleVariable` nodes, `DEFINES`, `INCLUDES_ROLE`, `NOTIFIES_HANDLER` and `CALLS` edges,
//! the cross-repository `DEPLOYS`/`HTTP_CALLS`/`CONFIGURES` targets of known modules, and the
//! URL/host/connection-string/queue references and play `hosts` (retained unresolved calls).
use super::{Out, file_stem, props, utf16_len, utf16_prefix};
use devworks_core::ports::indexing::{ExtractFault, Span};
use regex::Regex;
use serde_json::{Map, json};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;
use tree_sitter::Node;

// ---------------------------------------------------------------------------------------------
// The YAML representation model (YamlDotNet's `YamlStream` as the retained parser used it)

pub(crate) enum Y {
    Scalar(String),
    Seq(Vec<Rc<YNode>>),
    Map(Vec<(Rc<YNode>, Rc<YNode>)>),
}
pub(crate) struct YNode {
    pub(crate) y: Y,
    pub(crate) start: usize,
    pub(crate) end: usize,
}
impl YNode {
    fn scalar(&self) -> Option<&str> {
        match &self.y {
            Y::Scalar(s) => Some(s),
            _ => None,
        }
    }
    fn seq(&self) -> Option<&[Rc<YNode>]> {
        match &self.y {
            Y::Seq(s) => Some(s),
            _ => None,
        }
    }
    fn map(&self) -> Option<&[(Rc<YNode>, Rc<YNode>)]> {
        match &self.y {
            Y::Map(m) => Some(m),
            _ => None,
        }
    }
    /// `Children.TryGetValue(new YamlScalarNode(key))`.
    fn get(&self, key: &str) -> Option<&Rc<YNode>> {
        self.map()?
            .iter()
            .find(|(k, _)| k.scalar() == Some(key))
            .map(|(_, v)| v)
    }
    /// The retained `GetScalar`: the value of `key` when it is a scalar.
    fn get_scalar(&self, key: &str) -> Option<&str> {
        self.get(key)?.scalar()
    }
}

const MAX_YAML_DEPTH: usize = 512;

struct Builder<'s> {
    src: &'s [u8],
    anchors: HashMap<String, Rc<YNode>>,
}
impl Builder<'_> {
    fn text(&self, n: Node<'_>) -> &str {
        std::str::from_utf8(&self.src[n.start_byte()..n.end_byte().min(self.src.len())])
            .unwrap_or("")
    }
    /// A block scalar's header and lines with the empty lines that follow it: the grammar ends
    /// the node at its last content line unless the stream ends, and keep chomping (`|+`)
    /// keeps those lines.
    fn block_scalar_text(&self, n: Node<'_>) -> String {
        let mut end = n.end_byte().min(self.src.len());
        let rest = &self.src[end..];
        let first = rest.iter().position(|b| *b == b'\n');
        if let Some(i) = first.filter(|i| rest[..*i].iter().all(|b| *b == b'\r')) {
            end += i + 1;
            loop {
                let rest = &self.src[end..];
                let Some(i) = rest.iter().position(|b| *b == b'\n') else {
                    break;
                };
                if !rest[..i].iter().all(|b| matches!(b, b' ' | b'\t' | b'\r')) {
                    break;
                }
                end += i + 1;
            }
        }
        String::from_utf8_lossy(&self.src[n.start_byte()..end]).into_owned()
    }
    fn empty(n: Node<'_>) -> Rc<YNode> {
        Rc::new(YNode {
            y: Y::Scalar(String::new()),
            start: n.start_byte(),
            end: n.start_byte(),
        })
    }
    fn named<'t>(n: Node<'t>) -> Vec<Node<'t>> {
        let mut c = n.walk();
        n.named_children(&mut c)
            .filter(|x| x.kind() != "comment")
            .collect()
    }
    fn build(&mut self, n: Node<'_>, depth: usize) -> Result<Rc<YNode>, String> {
        if depth > MAX_YAML_DEPTH {
            return Err("YAML nesting too deep".into());
        }
        let d = depth + 1;
        let node = |y: Y| {
            Rc::new(YNode {
                y,
                start: n.start_byte(),
                end: n.end_byte(),
            })
        };
        Ok(match n.kind() {
            "document" => match Self::named(n)
                .into_iter()
                .find(|x| matches!(x.kind(), "block_node" | "flow_node"))
            {
                Some(c) => self.build(c, d)?,
                None => Self::empty(n),
            },
            "block_node" | "flow_node" => {
                let mut anchor = None;
                let mut content = None;
                for c in Self::named(n) {
                    match c.kind() {
                        "anchor" => anchor = Some(self.text(c).trim_start_matches('&').to_owned()),
                        "tag" => {}
                        _ => content = Some(c),
                    }
                }
                let value = match content {
                    Some(c) => self.build(c, d)?,
                    None => Self::empty(n),
                };
                if let Some(a) = anchor {
                    self.anchors.insert(a, value.clone());
                }
                value
            }
            "alias" => {
                let name = self.text(n).trim_start_matches('*');
                self.anchors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("alias *{name} names no anchor"))?
            }
            "block_mapping" | "flow_mapping" => {
                let mut entries = Vec::new();
                let mut keys = HashSet::new();
                for c in Self::named(n) {
                    let (k, v) = match c.kind() {
                        "block_mapping_pair" | "flow_pair" => (
                            match c.child_by_field_name("key") {
                                Some(k) => self.build(k, d)?,
                                None => Self::empty(c),
                            },
                            match c.child_by_field_name("value") {
                                Some(v) => self.build(v, d)?,
                                None => Self::empty(c),
                            },
                        ),
                        // `{a, b}`: a key without a value.
                        _ => (self.build(c, d)?, Self::empty(c)),
                    };
                    if let Some(s) = k.scalar()
                        && !keys.insert(s.to_owned())
                    {
                        return Err(format!("Duplicate key: {s}"));
                    }
                    entries.push((k, v));
                }
                node(Y::Map(entries))
            }
            "block_sequence" => {
                let mut items = Vec::new();
                for item in Self::named(n) {
                    items.push(match Self::named(item).into_iter().next() {
                        Some(c) => self.build(c, d)?,
                        None => Self::empty(item),
                    });
                }
                node(Y::Seq(items))
            }
            "flow_sequence" => {
                let mut items = Vec::new();
                for c in Self::named(n) {
                    if c.kind() == "flow_pair" {
                        // A single-pair mapping inside a flow sequence.
                        let k = match c.child_by_field_name("key") {
                            Some(k) => self.build(k, d)?,
                            None => Self::empty(c),
                        };
                        let v = match c.child_by_field_name("value") {
                            Some(v) => self.build(v, d)?,
                            None => Self::empty(c),
                        };
                        items.push(Rc::new(YNode {
                            y: Y::Map(vec![(k, v)]),
                            start: c.start_byte(),
                            end: c.end_byte(),
                        }));
                    } else {
                        items.push(self.build(c, d)?);
                    }
                }
                node(Y::Seq(items))
            }
            "plain_scalar" => node(Y::Scalar(fold(self.text(n)))),
            "single_quote_scalar" => {
                let t = self.text(n);
                let inner = t.get(1..t.len().saturating_sub(1)).unwrap_or("");
                node(Y::Scalar(fold(&inner.replace("''", "'"))))
            }
            "double_quote_scalar" => {
                let t = self.text(n);
                let inner = t.get(1..t.len().saturating_sub(1)).unwrap_or("");
                node(Y::Scalar(double_quoted(inner)))
            }
            "block_scalar" => node(Y::Scalar(block_scalar(
                &self.block_scalar_text(n),
                parent_indent(n),
            ))),
            other => return Err(format!("unexpected YAML node {other}")),
        })
    }
}

/// Flow scalar line folding: a single line break becomes a space, `n` empty lines become `n`
/// line feeds; continuation lines lose their leading and the previous line its trailing white
/// space.
fn fold(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    let mut out = String::new();
    let mut empties = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let mut l = l.trim_end_matches('\r');
        if i > 0 {
            l = l.trim_start_matches([' ', '\t']);
        }
        if i < last {
            l = l.trim_end_matches([' ', '\t']);
        }
        if i == 0 {
            out.push_str(l);
            continue;
        }
        if l.is_empty() && i < last {
            empties += 1;
            continue;
        }
        if empties > 0 {
            out.extend(std::iter::repeat_n('\n', empties));
        } else {
            out.push(' ');
        }
        empties = 0;
        out.push_str(l);
    }
    out
}

/// A double-quoted scalar's value: escapes decoded and lines folded.
fn double_quoted(inner: &str) -> String {
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let hex = |s: &[char]| -> Option<char> {
        u32::from_str_radix(&s.iter().collect::<String>(), 16)
            .ok()
            .and_then(char::from_u32)
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let e = chars[i + 1];
            i += 2;
            let simple = match e {
                '0' => Some('\0'),
                'a' => Some('\x07'),
                'b' => Some('\x08'),
                't' | '\t' => Some('\t'),
                'n' => Some('\n'),
                'v' => Some('\x0b'),
                'f' => Some('\x0c'),
                'r' => Some('\r'),
                'e' => Some('\x1b'),
                ' ' => Some(' '),
                '"' => Some('"'),
                '/' => Some('/'),
                '\\' => Some('\\'),
                'N' => Some('\u{85}'),
                '_' => Some('\u{a0}'),
                'L' => Some('\u{2028}'),
                'P' => Some('\u{2029}'),
                _ => None,
            };
            if let Some(s) = simple {
                out.push(s);
                continue;
            }
            let width = match e {
                'x' => 2,
                'u' => 4,
                'U' => 8,
                _ => 0,
            };
            if width > 0 && i + width <= chars.len() {
                if let Some(h) = hex(&chars[i..i + width]) {
                    out.push(h);
                }
                i += width;
                continue;
            }
            if e == '\n' || e == '\r' {
                // An escaped line break: the lines join without a space.
                while i < chars.len() && matches!(chars[i], ' ' | '\t' | '\n' | '\r') {
                    i += 1;
                }
                continue;
            }
            out.push(e);
            continue;
        }
        if c == '\n' || c == '\r' {
            while out.ends_with([' ', '\t']) {
                out.pop();
            }
            let mut breaks = 0;
            while i < chars.len() && matches!(chars[i], ' ' | '\t' | '\n' | '\r') {
                if chars[i] == '\n' {
                    breaks += 1;
                }
                i += 1;
            }
            if breaks > 1 {
                out.extend(std::iter::repeat_n('\n', breaks - 1));
            } else {
                out.push(' ');
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The indentation level of the collection entry a block scalar is the value of (YAML's `n`):
/// the column of its mapping key or sequence `-`; -1 at the top of a document.
fn parent_indent(n: Node<'_>) -> i64 {
    let mut cur = n.parent();
    while let Some(p) = cur {
        match p.kind() {
            "block_mapping_pair" | "block_sequence_item" => {
                return p.start_position().column as i64;
            }
            "document" | "stream" => return -1,
            _ => cur = p.parent(),
        }
    }
    -1
}

/// A block scalar (`|` literal, `>` folded) with its chomping (`-` strip, `+` keep, clip by
/// default) and indentation indicators, in either order; without an indentation indicator the
/// content indentation is the first non-empty line's, with one it is `parent_indent` plus the
/// indicator, so more-indented leading lines keep their extra spaces.
fn block_scalar(text: &str, parent_indent: i64) -> String {
    let mut lines = text.split('\n');
    let header = lines.next().unwrap_or("");
    let header = header.split(" #").next().unwrap_or(header).trim();
    let literal = header.starts_with('|');
    let indicators = header.get(1..).unwrap_or("");
    let chomp = if indicators.contains('-') {
        '-'
    } else if indicators.contains('+') {
        '+'
    } else {
        ' '
    };
    let explicit = indicators
        .chars()
        .find_map(|c| c.to_digit(10))
        .filter(|d| *d > 0);
    let body: Vec<&str> = lines.map(|l| l.trim_end_matches('\r')).collect();
    let indent = match explicit {
        Some(d) => (parent_indent + i64::from(d)).max(0) as usize,
        None => body
            .iter()
            .find(|l| !l.trim().is_empty())
            .map_or(0, |l| l.len() - l.trim_start_matches(' ').len()),
    };
    let content: Vec<&str> = body
        .iter()
        .map(|l| {
            let n = l.len() - l.trim_start_matches(' ').len();
            &l[n.min(indent)..]
        })
        .collect();
    // Trailing empty lines belong to chomping.
    let mut end = content.len();
    while end > 0 && content[end - 1].trim().is_empty() {
        end -= 1;
    }
    let trailing = content.len() - end;
    let kept = &content[..end];
    let mut out = String::new();
    if literal {
        out = kept.join("\n");
    } else {
        let mut prev_more = false;
        for (i, l) in kept.iter().enumerate() {
            let more = l.starts_with([' ', '\t']);
            if i > 0 {
                if l.is_empty() {
                    out.push('\n');
                    continue;
                }
                if more || prev_more || kept[i - 1].is_empty() {
                    if !kept[i - 1].is_empty() {
                        out.push('\n');
                    }
                } else {
                    out.push(' ');
                }
            }
            out.push_str(l);
            prev_more = more;
        }
    }
    if end == 0 {
        return if chomp == '+' {
            "\n".repeat(trailing)
        } else {
            String::new()
        };
    }
    match chomp {
        '-' => {}
        '+' => {
            out.push('\n');
            out.extend(std::iter::repeat_n('\n', trailing.saturating_sub(1)));
        }
        _ => out.push('\n'),
    }
    out
}

/// The first document of `root` (a Tree-sitter YAML stream), or why it cannot be read.
pub(crate) fn first_document(root: Node<'_>, src: &[u8]) -> Result<Option<Rc<YNode>>, String> {
    if root.has_error() {
        return Err("YAML syntax error".into());
    }
    let mut b = Builder {
        src,
        anchors: HashMap::new(),
    };
    let docs: Vec<Node<'_>> = Builder::named(root)
        .into_iter()
        .filter(|d| d.kind() == "document")
        .collect();
    let mut first = None;
    // Every document is loaded (an error in any fails the stream); the first is read.
    for (i, d) in docs.into_iter().enumerate() {
        let doc = b.build(d, 0)?;
        b.anchors.clear();
        if i == 0 {
            first = Some(doc);
        }
    }
    Ok(first)
}

// ---------------------------------------------------------------------------------------------
// Classification

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileType {
    NotAnsible,
    Playbook,
    Tasks,
    Handlers,
    Vars,
    Defaults,
    Meta,
    Requirements,
}

fn role_subdir(s: &str) -> Option<FileType> {
    Some(match s.to_lowercase().as_str() {
        "tasks" => FileType::Tasks,
        "handlers" => FileType::Handlers,
        "vars" => FileType::Vars,
        "defaults" => FileType::Defaults,
        "meta" => FileType::Meta,
        _ => return None,
    })
}

fn classify(path: &str, content: &str) -> FileType {
    let normalized = path.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();
    for i in 0..segments.len() {
        if !segments[i].eq_ignore_ascii_case("roles") {
            continue;
        }
        if i + 2 < segments.len() {
            return role_subdir(segments[i + 2]).unwrap_or(FileType::NotAnsible);
        }
    }
    if segments.len() >= 2
        && let Some(t) = role_subdir(segments[segments.len() - 2])
    {
        return t;
    }
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    if name.eq_ignore_ascii_case("requirements.yml")
        || name.eq_ignore_ascii_case("requirements.yaml")
    {
        return FileType::Requirements;
    }
    if content.contains("hosts:")
        && (content.contains("tasks:")
            || content.contains("roles:")
            || content.contains("pre_tasks:")
            || content.contains("post_tasks:"))
    {
        return FileType::Playbook;
    }
    let trimmed = content.trim_start();
    if [
        "- name:",
        "---\n- name:",
        "- include:",
        "- include_tasks:",
        "- import_tasks:",
        "---\n- include:",
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
    {
        return FileType::Tasks;
    }
    FileType::NotAnsible
}

/// The role a path belongs to: `roles/{role}/...`, or the repository itself for a standalone
/// role (a file directly in `tasks/`, `handlers/`, `vars/`, `defaults/` or `meta/`).
fn role_name(path: &str, repository: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();
    for i in 0..segments.len().saturating_sub(2) {
        if segments[i].eq_ignore_ascii_case("roles") {
            return Some(segments[i + 1].to_owned());
        }
    }
    if segments.len() >= 2 && role_subdir(segments[segments.len() - 2]).is_some() {
        return Some(repository.to_owned());
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Extraction

const NON_MODULE_KEYS: &[&str] = &[
    "name",
    "when",
    "register",
    "notify",
    "listen",
    "tags",
    "vars",
    "with_items",
    "with_dict",
    "with_fileglob",
    "loop",
    "loop_control",
    "until",
    "retries",
    "delay",
    "changed_when",
    "failed_when",
    "ignore_errors",
    "become",
    "become_user",
    "become_method",
    "environment",
    "no_log",
    "check_mode",
    "diff",
    "any_errors_fatal",
    "block",
    "rescue",
    "always",
    "include_role",
    "import_role",
    "include_tasks",
    "import_tasks",
    "include_vars",
    "set_fact",
    "delegate_to",
    "run_once",
    "connection",
    "timeout",
];

fn re(p: &str) -> Regex {
    Regex::new(p).expect("a valid retained expression")
}
static URL: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)https?://[\w\-\.]+(?::\d+)?(?:/[\w\-\./%]*)?"));
static HOST_PORT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(?<host>[\w\-\.]+\.(?:internal|local|corp|svc|cluster\.local|com|net|io))(?::(?<port>\d+))?",
    )
});
static CONNECTION_STRING: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(?:Server|Data Source|Host)=(?<host>[^;]+)"));
static QUEUE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)(?:queue|exchange|topic)[\w\-\s]*[:=]\s*["']?(?<name>[\w\-\.]+)"#));

/// The retained `Sanitize` (lowercase; space, `-` and `.` to `_`).
fn sanitize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if matches!(c, ' ' | '-' | '.') { '_' } else { c })
        .collect()
}

pub(crate) fn extract(
    path: &str,
    source: &[u8],
    root: Node<'_>,
    repository: &str,
    out: &mut Out<'_>,
) -> Result<(), ExtractFault> {
    let content = String::from_utf8_lossy(source);
    if content.trim().is_empty() {
        return Ok(());
    }
    let file_type = classify(path, &content);
    if file_type == FileType::NotAnsible {
        return Ok(());
    }
    let doc = match first_document(root, source) {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(why) => {
            out.failed(format!("ansible: {why}"));
            return Ok(());
        }
    };
    let lines = super::Lines::new(&content);
    let mut a = Ansible {
        path,
        repository,
        lines: &lines,
        out,
        discovered: HashSet::new(),
    };
    match file_type {
        FileType::Playbook => a.playbook(&doc)?,
        FileType::Tasks => {
            let parent = a.parent_qn();
            a.task_list(&doc, &parent)?;
        }
        FileType::Handlers => a.handler_list(&doc)?,
        FileType::Vars | FileType::Defaults => a.variables(&doc, file_type)?,
        FileType::Meta => a.role_meta(&doc),
        FileType::Requirements => a.requirements(&doc),
        FileType::NotAnsible => {}
    }
    a.scan_references(&doc)
}

struct Ansible<'c, 'o, 'a> {
    path: &'c str,
    repository: &'c str,
    lines: &'c super::Lines,
    out: &'o mut Out<'a>,
    discovered: HashSet<String>,
}

impl Ansible<'_, '_, '_> {
    fn span(&self, n: &YNode) -> Span {
        self.lines.span(n.start, n.end)
    }
    fn role(&self) -> Option<String> {
        role_name(self.path, self.repository)
    }
    /// `ParentQN`: the role, or the file as a task file.
    fn parent_qn(&self) -> String {
        match self.role() {
            Some(r) => format!("role.{r}"),
            None => format!("taskfile.{}", file_stem(self.path)),
        }
    }

    fn playbook(&mut self, root: &Rc<YNode>) -> Result<(), ExtractFault> {
        let Some(plays) = root.seq() else {
            return Ok(());
        };
        let name = file_stem(self.path).to_owned();
        let qn = format!("playbook.{name}");
        let span = self.span(root);
        self.out.node(
            "Playbook",
            &name,
            &qn,
            span,
            None,
            props(&[("type", json!("playbook"))]),
        );
        for play in plays {
            self.out.tick()?;
            if play.map().is_none() {
                continue;
            }
            let hosts = play.get_scalar("hosts").unwrap_or("unknown").to_owned();
            if let Some(roles) = play.get("roles").filter(|r| r.seq().is_some()) {
                self.playbook_roles(roles, &qn);
            }
            for key in ["tasks", "pre_tasks", "post_tasks"] {
                if let Some(tasks) = play.get(key).filter(|t| t.seq().is_some()) {
                    self.task_list(tasks, &qn)?;
                }
            }
            if let Some(handlers) = play.get("handlers").filter(|h| h.seq().is_some()) {
                self.handler_list(handlers)?;
            }
            self.out.unresolved_call(&qn, &hosts, "ansible_hosts", 0.5);
        }
        Ok(())
    }

    fn playbook_roles(&mut self, roles: &YNode, parent: &str) {
        for r in roles.seq().unwrap_or_default() {
            let name = match &r.y {
                Y::Scalar(s) => Some(s.as_str()),
                Y::Map(_) => r.get_scalar("role").or_else(|| r.get_scalar("name")),
                Y::Seq(_) => None,
            };
            let Some(name) = name.filter(|n| !n.is_empty()) else {
                continue;
            };
            let qn = format!("role.{name}");
            let span = self.span(r);
            self.out.node(
                "Role",
                name,
                &qn,
                span,
                None,
                props(&[("confidence", json!("medium"))]),
            );
            self.out.edge("INCLUDES_ROLE", parent, &qn, Map::new());
        }
    }

    fn task_list(&mut self, root: &YNode, parent: &str) -> Result<(), ExtractFault> {
        let Some(tasks) = root.seq() else {
            return Ok(());
        };
        for task in tasks {
            self.out.tick()?;
            if task.map().is_none() {
                continue;
            }
            if task.get("block").is_some() {
                for key in ["block", "rescue", "always"] {
                    if let Some(list) = task.get(key) {
                        self.task_list(list, parent)?;
                    }
                }
                continue;
            }
            self.single_task(task, parent);
        }
        Ok(())
    }

    fn single_task(&mut self, task: &YNode, parent: &str) {
        let name = task.get_scalar("name").unwrap_or("unnamed_task").to_owned();
        let module = detect_module(task);
        let qn = format!("{parent}.task.{}", sanitize(&name));
        let mut p = props(&[("confidence", json!("high"))]);
        if let Some(m) = &module {
            p.insert("module".into(), json!(m));
        }
        let parent_index = self.out.index_of(parent);
        let span = self.span(task);
        self.out
            .node("AnsibleTask", &name, &qn, span, parent_index, p);
        self.out.contain("DEFINES", parent, &qn);

        if let Some(notify) = task.get("notify") {
            let handlers: Vec<&str> = match &notify.y {
                Y::Scalar(s) => vec![s.as_str()],
                Y::Seq(items) => items.iter().filter_map(|i| i.scalar()).collect(),
                Y::Map(_) => Vec::new(),
            };
            for h in handlers.into_iter().filter(|h| !h.is_empty()) {
                let target = format!("handler.{}", sanitize(h));
                self.out.edge("NOTIFIES_HANDLER", &qn, &target, Map::new());
            }
        }
        for key in ["include_role", "import_role"] {
            if let Some(include) = task.get(key).filter(|i| i.map().is_some())
                && let Some(role) = include.get_scalar("name").filter(|r| !r.is_empty())
            {
                let role_qn = format!("role.{role}");
                let span = self.span(include);
                self.out
                    .node("Role", role, &role_qn, span, None, Map::new());
                self.out.edge("INCLUDES_ROLE", &qn, &role_qn, Map::new());
            }
        }
        for key in ["include_tasks", "import_tasks"] {
            if let Some(file) = task.get_scalar(key).filter(|f| !f.is_empty()) {
                let target = format!("{parent}.taskfile.{}", sanitize(file_stem(file)));
                self.out.edge("CALLS", &qn, &target, Map::new());
            }
        }
        self.cross_repo_edges(task, module.as_deref(), &qn);
    }

    fn handler_list(&mut self, root: &YNode) -> Result<(), ExtractFault> {
        let Some(handlers) = root.seq() else {
            return Ok(());
        };
        for h in handlers {
            self.out.tick()?;
            if h.map().is_none() {
                continue;
            }
            let name = h.get_scalar("name").unwrap_or("unnamed_handler").to_owned();
            let module = detect_module(h);
            let qn = format!("handler.{}", sanitize(&name));
            let mut p = props(&[("confidence", json!("high"))]);
            if let Some(m) = &module {
                p.insert("module".into(), json!(m));
            }
            if let Some(l) = h.get_scalar("listen").filter(|l| !l.is_empty()) {
                p.insert("listen".into(), json!(l));
            }
            let span = self.span(h);
            self.out.node("AnsibleHandler", &name, &qn, span, None, p);
            self.cross_repo_edges(h, module.as_deref(), &qn);
        }
        Ok(())
    }

    fn variables(&mut self, root: &YNode, file_type: FileType) -> Result<(), ExtractFault> {
        let Some(vars) = root.map() else {
            return Ok(());
        };
        let role = self
            .role()
            .unwrap_or_else(|| file_stem(self.path).to_owned());
        let scope = if file_type == FileType::Defaults {
            "default"
        } else {
            "var"
        };
        for (k, v) in vars {
            self.out.tick()?;
            let Some(name) = k.scalar() else { continue };
            let qn = format!("{scope}.{role}.{name}");
            let mut p = props(&[("scope", json!(scope)), ("confidence", json!("high"))]);
            if let Some(value) = v.scalar() {
                let preview = if utf16_len(value) > 100 {
                    format!("{}...", utf16_prefix(value, 100))
                } else {
                    value.to_owned()
                };
                p.insert("value_preview".into(), json!(preview));
                if value.contains("://")
                    || value.contains(":5")
                    || value.contains(":8")
                    || value.contains(":3")
                {
                    p.insert("likely_service_ref".into(), json!(true));
                }
            }
            let span = self.lines.span(k.start, v.end.max(k.end));
            self.out.node("AnsibleVariable", name, &qn, span, None, p);
        }
        Ok(())
    }

    fn role_meta(&mut self, root: &YNode) {
        if root.map().is_none() {
            return;
        }
        let role = self
            .role()
            .unwrap_or_else(|| file_stem(self.path).to_owned());
        let qn = format!("role.{role}");
        let span = self.span(root);
        self.out.node(
            "Role",
            &role,
            &qn,
            span,
            None,
            props(&[("confidence", json!("high"))]),
        );
        if let Some(deps) = root.get("dependencies").and_then(|d| d.seq()) {
            for d in deps {
                let name = match &d.y {
                    Y::Scalar(s) => Some(s.as_str()),
                    Y::Map(_) => d.get_scalar("role").or_else(|| d.get_scalar("name")),
                    Y::Seq(_) => None,
                };
                let Some(name) = name.filter(|n| !n.is_empty()) else {
                    continue;
                };
                let dep = format!("role.{name}");
                let span = self.span(d);
                self.out.node("Role", name, &dep, span, None, Map::new());
                self.out.edge("INCLUDES_ROLE", &qn, &dep, Map::new());
            }
        }
        if let Some(galaxy) = root.get("galaxy_info").filter(|g| g.map().is_some())
            && let Some(desc) = galaxy.get_scalar("description").filter(|d| !d.is_empty())
        {
            self.out.node(
                "Role",
                &role,
                &qn,
                span,
                None,
                props(&[("description", json!(desc)), ("confidence", json!("high"))]),
            );
        }
    }

    fn requirements(&mut self, root: &YNode) {
        let roles = match &root.y {
            Y::Seq(_) => Some(root),
            Y::Map(_) => root.get("roles").map(|r| r.as_ref()),
            Y::Scalar(_) => None,
        };
        let Some(roles) = roles.and_then(|r| r.seq()) else {
            return;
        };
        for r in roles {
            let (name, src) = match &r.y {
                Y::Scalar(s) => (Some(s.as_str()), None),
                Y::Map(_) => (
                    r.get_scalar("name").or_else(|| r.get_scalar("role")),
                    r.get_scalar("src"),
                ),
                Y::Seq(_) => (None, None),
            };
            if name.is_none_or(str::is_empty) && src.is_none_or(str::is_empty) {
                continue;
            }
            let Some(name) = name.or(src) else { continue };
            let qn = format!("role.{name}");
            let mut p = props(&[("confidence", json!("medium"))]);
            if let Some(s) = src {
                p.insert("source".into(), json!(s));
            }
            let span = self.span(r);
            self.out.node("Role", name, &qn, span, None, p);
        }
    }

    /// `ExtractCrossRepoEdges`: deployment and configuration targets of known modules.
    fn cross_repo_edges(&mut self, task: &YNode, module: Option<&str>, qn: &str) {
        let Some(module) = module else { return };
        let arg = |name: &str| -> Option<String> {
            task.get(module)
                .filter(|m| m.map().is_some())
                .and_then(|m| m.get_scalar(name))
                .map(str::to_owned)
        };
        let medium = json!("medium");
        match module {
            "service" | "systemd" | "win_service" => {
                if let Some(s) = arg("name").filter(|s| !s.is_empty()) {
                    let p = props(&[("service_name", json!(s)), ("confidence", medium)]);
                    self.out.name_edge("DEPLOYS", qn, &s, p);
                }
            }
            "uri" | "win_uri" => {
                if let Some(u) = arg("url").filter(|s| !s.is_empty()) {
                    let p = props(&[("url", json!(u)), ("confidence", json!("low"))]);
                    self.out.name_edge("HTTP_CALLS", qn, &u, p);
                }
            }
            "template" | "copy" => {
                if let Some(d) = arg("dest").filter(|s| !s.is_empty()) {
                    let p = props(&[("destination", json!(d)), ("confidence", medium)]);
                    self.out.name_edge("CONFIGURES", qn, &d, p);
                }
            }
            "docker_container" | "community.docker.docker_container" => {
                let image = arg("image");
                if let Some(c) = arg("name").filter(|s| !s.is_empty()) {
                    let mut p = props(&[("container", json!(c)), ("confidence", medium)]);
                    if let Some(i) = image {
                        p.insert("image".into(), json!(i));
                    }
                    self.out.name_edge("DEPLOYS", qn, &c, p);
                }
            }
            "win_iis_website" | "community.windows.win_iis_website" => {
                if let Some(s) = arg("name").filter(|s| !s.is_empty()) {
                    let p = props(&[("iis_site", json!(s)), ("confidence", medium)]);
                    self.out.name_edge("DEPLOYS", qn, &s, p);
                }
            }
            "mysql_db" | "community.mysql.mysql_db" => {
                if let Some(d) = arg("name").filter(|s| !s.is_empty()) {
                    let p = props(&[("database", json!(d)), ("confidence", medium)]);
                    self.out.name_edge("CONFIGURES", qn, &d, p);
                }
            }
            _ => {}
        }
    }

    /// `ScanForReferences`: URLs, hosts, connection strings and queue names in every scalar
    /// (and every `key: value` pair) of the document.
    fn scan_references(&mut self, root: &YNode) -> Result<(), ExtractFault> {
        let parent = self.parent_qn();
        let mut values = Vec::new();
        collect_scalars(root, &mut values);
        for v in &values {
            self.out.tick()?;
            for m in URL.find_iter(v) {
                if self.discovered.insert(format!("url:{}", m.as_str())) {
                    self.out
                        .unresolved_call(&parent, m.as_str(), "ansible_url_ref", 0.4);
                }
            }
            for c in HOST_PORT.captures_iter(v) {
                let host = &c["host"];
                if self.discovered.insert(format!("host:{host}")) {
                    self.out
                        .unresolved_call(&parent, host, "ansible_host_ref", 0.3);
                }
            }
            for c in CONNECTION_STRING.captures_iter(v) {
                let host = &c["host"];
                if self.discovered.insert(format!("connstr:{host}")) {
                    self.out
                        .unresolved_call(&parent, host, "ansible_connection_ref", 0.5);
                }
            }
            for c in QUEUE.captures_iter(v) {
                let name = &c["name"];
                if self.discovered.insert(format!("queue:{name}")) {
                    self.out
                        .unresolved_call(&parent, name, "ansible_queue_ref", 0.5);
                }
            }
        }
        Ok(())
    }
}

/// `DetectModule`: the first key that is not a task keyword and whose value is a mapping or a
/// scalar.
fn detect_module(task: &YNode) -> Option<String> {
    for (k, v) in task.map()? {
        let Some(key) = k.scalar() else { continue };
        if NON_MODULE_KEYS.iter().any(|n| n.eq_ignore_ascii_case(key)) {
            continue;
        }
        if matches!(v.y, Y::Map(_) | Y::Scalar(_)) {
            return Some(key.to_owned());
        }
    }
    None
}

/// `CollectScalarValues`: every scalar, and `key: value` for every scalar pair, in document
/// order (keys themselves are not collected).
fn collect_scalars(root: &YNode, out: &mut Vec<String>) {
    enum Item<'n> {
        Node(&'n YNode),
        Pair(String),
    }
    let mut stack = vec![Item::Node(root)];
    while let Some(item) = stack.pop() {
        let n = match item {
            Item::Pair(text) => {
                out.push(text);
                continue;
            }
            Item::Node(n) => n,
        };
        match &n.y {
            Y::Scalar(s) => out.push(s.clone()),
            Y::Seq(items) => stack.extend(items.iter().rev().map(|i| Item::Node(i))),
            Y::Map(entries) => {
                // Each pair's `key: value` text, then its value, in order.
                for (k, v) in entries.iter().rev() {
                    stack.push(Item::Node(v));
                    if let (Some(k), Some(v)) = (k.scalar(), v.scalar()) {
                        stack.push(Item::Pair(format!("{k}: {v}")));
                    }
                }
            }
        }
        // Aliases can repeat a subtree many times: the walk is bounded.
        if out.len() > 1_000_000 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::first_document;

    /// The scalar at `path` in the first document (a key, or `None` for a first sequence item).
    fn value(src: &str, path: &[Option<&str>]) -> String {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&tree_sitter_yaml::LANGUAGE.into())
            .expect("the YAML grammar");
        let tree = p.parse(src, None).expect("a tree");
        let mut node = first_document(tree.root_node(), src.as_bytes())
            .expect("valid YAML")
            .expect("a document");
        for step in path {
            node = match step {
                Some(k) => node.get(k).expect("the key").clone(),
                None => node.seq().expect("a sequence")[0].clone(),
            };
        }
        node.scalar().expect("a scalar").to_owned()
    }

    #[test]
    fn block_scalars_keep_trailing_lines_and_honour_indentation_indicators() {
        let a = &[Some("a")];
        // Keep chomping keeps the empty lines after the last content line, before more content
        // and at the end of the stream; clip and strip do not.
        assert_eq!(value("a: |+\n  x\n\n\nb: 1\n", a), "x\n\n\n");
        assert_eq!(value("a: |+\n  x\n\n\n", a), "x\n\n\n");
        assert_eq!(value("- >+\n  x\n  y\n\n- z\n", &[None]), "x y\n\n");
        assert_eq!(
            value("k:\n  a: |+\n    x\n\n  b: 1\n", &[Some("k"), Some("a")]),
            "x\n\n"
        );
        assert_eq!(value("a: |\n  x\n\n\nb: 1\n", a), "x\n");
        assert_eq!(value("a: |-\n  x\n\n\nb: 1\n", a), "x");
        // An indentation indicator counts from the entry's indentation: more-indented leading
        // lines keep their extra spaces, in either indicator order.
        assert_eq!(value("a: |2\n    x\n  y\nb: 1\n", a), "  x\ny\n");
        assert_eq!(value("a: |2-\n    x\n  y\n", a), "  x\ny");
        assert_eq!(value("a: |-2\n    x\n  y\n", a), "  x\ny");
        assert_eq!(
            value("k:\n  a: |2\n      x\n    y\n", &[Some("k"), Some("a")]),
            "  x\ny\n"
        );
        assert_eq!(value("- |1\n  explicit\n", &[None]), " explicit\n");
        assert_eq!(value("a: >1\n  x\n y\n", a), " x\ny\n");
    }
}
