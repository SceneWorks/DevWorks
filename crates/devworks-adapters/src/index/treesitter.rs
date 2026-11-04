//! Tree-sitter structural extraction (CODE-01/02), retaining the CodeGraph `LanguageSpecs`
//! surface (C, C++, Python, Go, Java, Ruby, Rust, PHP, Bash, Swift) and the frozen adapter
//! languages with maintained Rust grammar crates (C#, JavaScript/JSX, TypeScript/TSX, SQL,
//! Terraform/HCL, Ansible/YAML, ColdFusion). Each extractor reports what it supports in its
//! [`LanguageInfo::capabilities`]; nothing here claims compiler semantics.
//!
//! The `LanguageSpecs` languages run the retained generic walk ([`spec`], with the Swift call-site
//! reader in [`swift`]): retained labels, qualified names (without the `{repository}:{path}`
//! prefix), properties, call receivers and confidences, argument labels, ABI, Swift inheritance
//! and the Swift tree-sitter fallback marking. C#, JavaScript and TypeScript (retained by other
//! CodeGraph extractors, not by `LanguageSpecs`) keep the structural walk below: a `File` node
//! (index 0), definitions with 1-based spans and qualified names (`Outer.Inner.member`),
//! `DEFINES`/`DEFINES_METHOD` containment, `IMPORTS` to the imported module name, `CALLS` from the
//! enclosing callable, and inheritance (`INHERITS`/`IMPLEMENTS`). A relationship whose target is
//! a unique definition of the same file is `Inferred` locally; any other is a symbol for the
//! indexer to link or leave `Unresolved`.
//!
//! Parsing polls the run's stop signal and a per-file budget through Tree-sitter's progress
//! callback, so a cancelled run stops within one callback interval. Walks are iterative
//! (no recursion on deep trees) and bounded in nodes and edges.
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedEdge, ExtractedNode, FileExtract, LanguageInfo,
    LanguageMode, Resolution, Span, StructuralParser,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};
use tree_sitter::{Language, Node, Parser};

mod spec;
mod swift;

/// Most nodes and edges kept per file (beyond them the file is recorded partial).
const MAX_NODES: usize = 10_000;
const MAX_EDGES: usize = 40_000;
/// Deepest syntax nesting walked.
const MAX_DEPTH: usize = 2_000;

/// The structural walk of C#, JavaScript and TypeScript.
const FULL_INHERIT: &[&str] = &[
    "definitions",
    "spans",
    "containment",
    "imports",
    "calls",
    "inheritance",
];
// The retained `LanguageSpecs` surface per language ([`spec`]): Go and PHP specs
// name import nodes the retained import reader reads nothing from, Ruby and Bash have none;
// inheritance edges are Swift's only (the others record a `superclass` property).
const C_CAPS: &[&str] = &["definitions", "spans", "containment", "imports", "calls"];
const CPP_CAPS: &[&str] = &[
    "definitions",
    "spans",
    "containment",
    "imports",
    "calls",
    "abi",
];
const PY_JAVA_CAPS: &[&str] = &[
    "definitions",
    "spans",
    "containment",
    "imports",
    "calls",
    "superclass",
];
const GO_PHP_CAPS: &[&str] = &["definitions", "spans", "containment", "calls"];
const RUBY_CAPS: &[&str] = &["definitions", "spans", "containment", "calls", "superclass"];
const RUST_CAPS: &[&str] = &[
    "definitions",
    "spans",
    "containment",
    "imports",
    "calls",
    "abi",
];
const BASH_CAPS: &[&str] = &["definitions", "spans", "containment", "calls"];
const SWIFT_CAPS: &[&str] = &[
    "definitions",
    "spans",
    "containment",
    "imports",
    "calls",
    "argument-labels",
    "properties",
    "inheritance",
    "tree-sitter-fallback-tier",
];

macro_rules! lang {
    ($id:expr, $name:expr, $ext:expr, $grammar:expr, $caps:expr) => {
        LanguageInfo {
            id: $id,
            name: $name,
            extensions: $ext,
            mode: LanguageMode::Structural,
            grammar: Some($grammar),
            capabilities: $caps,
            note: None,
        }
    };
}
macro_rules! text_only {
    ($id:expr, $name:expr, $ext:expr, $note:expr) => {
        LanguageInfo {
            id: $id,
            name: $name,
            extensions: $ext,
            mode: LanguageMode::TextFallback,
            grammar: None,
            capabilities: &[],
            note: Some($note),
        }
    };
}

/// Every language this build knows: the grammar languages, the text-only ones and the
/// manifests (see [`super::manifests`]).
pub fn languages() -> Vec<LanguageInfo> {
    let mut all = grammar_languages();
    all.extend(super::manifests::languages());
    all
}

fn grammar_languages() -> Vec<LanguageInfo> {
    vec![
        lang!("c", "C", &[".c", ".h"], "tree-sitter-c 0.24.2", C_CAPS),
        lang!(
            "cpp",
            "C++",
            &[".cpp", ".cc", ".cxx", ".hpp", ".hxx", ".hh"],
            "tree-sitter-cpp 0.23.4",
            CPP_CAPS
        ),
        lang!(
            "csharp",
            "C#",
            &[".cs"],
            "tree-sitter-c-sharp 0.23.5",
            FULL_INHERIT
        ),
        lang!(
            "python",
            "Python",
            &[".py", ".pyw"],
            "tree-sitter-python 0.25.0",
            PY_JAVA_CAPS
        ),
        lang!("go", "Go", &[".go"], "tree-sitter-go 0.25.0", GO_PHP_CAPS),
        lang!(
            "java",
            "Java",
            &[".java"],
            "tree-sitter-java 0.23.5",
            PY_JAVA_CAPS
        ),
        lang!(
            "ruby",
            "Ruby",
            &[".rb"],
            "tree-sitter-ruby 0.23.1",
            RUBY_CAPS
        ),
        lang!(
            "rust",
            "Rust",
            &[".rs"],
            "tree-sitter-rust 0.24.2",
            RUST_CAPS
        ),
        lang!(
            "php",
            "PHP",
            &[".php"],
            "tree-sitter-php 0.25.1",
            GO_PHP_CAPS
        ),
        lang!(
            "bash",
            "Bash",
            &[".sh", ".bash", ".zsh"],
            "tree-sitter-bash 0.25.1",
            BASH_CAPS
        ),
        lang!(
            "swift",
            "Swift",
            &[".swift"],
            "tree-sitter-swift 0.7.4",
            SWIFT_CAPS
        ),
        lang!(
            "javascript",
            "JavaScript",
            &[".js", ".jsx", ".mjs", ".cjs"],
            "tree-sitter-javascript 0.25.0",
            FULL_INHERIT
        ),
        lang!(
            "typescript",
            "TypeScript",
            &[".ts", ".mts", ".cts"],
            "tree-sitter-typescript 0.23.2",
            FULL_INHERIT
        ),
        lang!(
            "tsx",
            "TSX",
            &[".tsx"],
            "tree-sitter-typescript 0.23.2",
            FULL_INHERIT
        ),
        lang!(
            "sql",
            "SQL",
            &[".sql"],
            "tree-sitter-sequel 0.3.11",
            // The retained SqlExtractor/SqlGraphVisitor (super::domain::sql).
            &[
                "definitions",
                "spans",
                "columns",
                "primary-keys",
                "foreign-keys",
                "indexes",
                "parameters",
                "view-queries",
                "procedure-queries",
                "procedure-calls",
                "mysql-normalization"
            ]
        ),
        lang!(
            "terraform",
            "Terraform/HCL",
            &[".tf", ".tfvars", ".hcl"],
            "tree-sitter-hcl 1.1.0",
            // The retained HclParser (super::domain::hcl).
            &[
                "definitions",
                "spans",
                "modules",
                "depends-on",
                "output-references",
                "locals",
                "tfvars",
                "deployment-targets",
                "service-references"
            ]
        ),
        lang!(
            "yaml",
            "YAML/Ansible",
            &[".yml", ".yaml"],
            "tree-sitter-yaml 0.7.2",
            // The retained AnsibleParser (super::domain::ansible); other YAML yields its file node.
            &[
                "ansible-playbooks",
                "ansible-tasks",
                "ansible-handlers",
                "ansible-roles",
                "ansible-role-dependencies",
                "ansible-requirements",
                "ansible-variables",
                "deployment-targets",
                "service-references"
            ]
        ),
        lang!(
            "coldfusion",
            "ColdFusion",
            &[".cfm", ".cfc", ".cfml"],
            "tree-sitter-cfml 0.26.44",
            // The retained ColdFusionParser (super::domain::cfml).
            &[
                "definitions",
                "spans",
                "containment",
                "inheritance",
                "calls",
                "object-creation",
                "http-calls",
                "queries",
                "imports"
            ]
        ),
        // Code languages outside the retained LanguageSpecs with no grammar in this build: their
        // files are indexed as text only, never claimed as structurally parsed.
        text_only!(
            "kotlin",
            "Kotlin",
            &[".kt", ".kts"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "scala",
            "Scala",
            &[".scala", ".sc"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "objective-c",
            "Objective-C",
            &[".m", ".mm"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "dart",
            "Dart",
            &[".dart"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "lua",
            "Lua",
            &[".lua"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "perl",
            "Perl",
            &[".pl", ".pm"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "elixir",
            "Elixir",
            &[".ex", ".exs"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "haskell",
            "Haskell",
            &[".hs"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "fsharp",
            "F#",
            &[".fs", ".fsx", ".fsi"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "visual-basic",
            "Visual Basic",
            &[".vb"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "powershell",
            "PowerShell",
            &[".ps1", ".psm1"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "vue",
            "Vue",
            &[".vue"],
            "no grammar in this build: text only"
        ),
        text_only!(
            "svelte",
            "Svelte",
            &[".svelte"],
            "no grammar in this build: text only"
        ),
    ]
}

fn grammar(id: &str) -> Option<Language> {
    Some(match id {
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "sql" => tree_sitter_sequel::LANGUAGE.into(),
        "terraform" => tree_sitter_hcl::LANGUAGE.into(),
        "yaml" => tree_sitter_yaml::LANGUAGE.into(),
        "coldfusion" => tree_sitter_cfml::LANGUAGE_CFML.into(),
        "cfscript" => tree_sitter_cfml::LANGUAGE_CFSCRIPT.into(),
        _ => return None,
    })
}

/// The Tree-sitter parser.
pub struct TreeSitterParser {
    languages: Vec<LanguageInfo>,
    by_extension: HashMap<&'static str, usize>,
    /// Exact file names (`Cargo.toml`, `go.mod`), matched before extensions.
    by_name: HashMap<&'static str, usize>,
}
impl Default for TreeSitterParser {
    fn default() -> Self {
        Self::new()
    }
}
impl TreeSitterParser {
    pub fn new() -> Self {
        let languages = languages();
        let mut by_extension = HashMap::new();
        let mut by_name = HashMap::new();
        for (i, l) in languages.iter().enumerate() {
            for e in l.extensions {
                if e.starts_with('.') {
                    by_extension.insert(*e, i);
                } else {
                    by_name.insert(*e, i);
                }
            }
        }
        Self {
            languages,
            by_extension,
            by_name,
        }
    }
}

impl StructuralParser for TreeSitterParser {
    fn languages(&self) -> &[LanguageInfo] {
        &self.languages
    }
    fn language_of(&self, path: &str) -> Option<&LanguageInfo> {
        let name = path.rsplit('/').next().unwrap_or(path);
        if let Some(i) = self.by_name.get(name) {
            return Some(&self.languages[*i]);
        }
        let dot = name.rfind('.')?;
        if dot == 0 {
            return None;
        }
        let ext = name[dot..].to_ascii_lowercase();
        self.by_extension
            .get(ext.as_str())
            .map(|i| &self.languages[*i])
    }
    fn extract(
        &self,
        language: &str,
        path: &str,
        repository: &str,
        source: &[u8],
        stop: &(dyn Fn() -> bool + Sync),
        budget_ms: u64,
    ) -> Result<FileExtract, ExtractFault> {
        if source.iter().take(8192).any(|b| *b == 0) {
            return Err(ExtractFault::Failed("binary content".into()));
        }
        if let Some(x) = super::manifests::extract_file(language, path, source) {
            return Ok(x);
        }
        // A tag-free ColdFusion component is CFScript.
        let grammar_id = if language == "coldfusion"
            && path.to_ascii_lowercase().ends_with(".cfc")
            && cfscript(source)
        {
            "cfscript"
        } else {
            language
        };
        let Some(lang) = grammar(grammar_id) else {
            return Err(ExtractFault::Failed(format!("no grammar for {language}")));
        };
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| ExtractFault::Failed(format!("grammar ABI: {e}")))?;
        let deadline = Instant::now() + Duration::from_millis(budget_ms.max(1));
        let mut timed_out = false;
        let mut cancelled = false;
        let mut progress = |_: &tree_sitter::ParseState| {
            if stop() {
                cancelled = true;
                return ControlFlow::Break(());
            }
            if Instant::now() >= deadline {
                timed_out = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        };
        let len = source.len();
        let tree = parser.parse_with_options(
            &mut |offset, _| {
                if offset < len {
                    &source[offset..]
                } else {
                    &[] as &[u8]
                }
            },
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
        );
        let Some(tree) = tree else {
            return Err(if cancelled {
                ExtractFault::Cancelled
            } else if timed_out {
                ExtractFault::TimedOut
            } else {
                ExtractFault::Failed("the parser produced no tree".into())
            });
        };
        let root = tree.root_node();
        let mut w = Walk::new(path, source, root, language);
        match grammar_id {
            "sql" => {
                super::domain::run(&mut w.out, stop, |o| super::domain::sql::extract(source, o))?
            }
            "terraform" => super::domain::run(&mut w.out, stop, |o| {
                super::domain::hcl::extract(path, source, o)
            })?,
            "yaml" => super::domain::run(&mut w.out, stop, |o| {
                super::domain::ansible::extract(path, source, root, repository, o)
            })?,
            "coldfusion" | "cfscript" => super::domain::run(&mut w.out, stop, |o| {
                super::domain::cfml::extract(path, source, o)
            })?,
            _ => match spec::spec(grammar_id) {
                Some(s) => spec::extract(s, source, path, root, &mut w.out, stop, deadline)?,
                None => w.walk(root, grammar_id, stop, deadline)?,
            },
        }
        if stop() {
            return Err(ExtractFault::Cancelled);
        }
        let mut x = w.finish();
        if matches!(grammar_id, "rust" | "go") {
            let e = super::endpoints::extract(root, source, path, grammar_id == "rust");
            let base = x.nodes.len();
            x.nodes.extend(e.nodes);
            x.edges.extend(e.edges.into_iter().map(|mut edge| {
                edge.source += base;
                edge
            }));
            x.incomplete.extend(e.incomplete);
        }
        Ok(x)
    }
    fn group_of(&self, path: &str) -> Option<&'static str> {
        super::manifests::group_of(path)
    }
    fn extract_group(
        &self,
        group: &str,
        repository: &str,
        files: &[(String, Vec<u8>)],
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<std::collections::BTreeMap<String, FileExtract>, ExtractFault> {
        super::manifests::extract_group(group, repository, files, stop)
    }
}

fn cfscript(source: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&source[..source.len().min(4096)]);
    let mut t = text.trim_start();
    while let Some(rest) = t.strip_prefix("/*") {
        t = rest.split_once("*/").map_or("", |(_, r)| r).trim_start();
    }
    while let Some(rest) = t.strip_prefix("//") {
        t = rest.split_once('\n').map_or("", |(_, r)| r).trim_start();
    }
    let lower = t.to_ascii_lowercase();
    lower.starts_with("component") || lower.starts_with("interface") || lower.starts_with("import ")
}

// ---------------------------------------------------------------------------------------------
// The generic walk

#[derive(Clone, Copy, PartialEq, Eq)]
enum Receiver {
    None,
    SelfRef,
    Other,
}

struct Call {
    caller: usize,
    class: Option<usize>,
    name: String,
    receiver: Receiver,
}

#[derive(Clone)]
struct Ctx {
    container: usize,
    class: Option<usize>,
    function: Option<usize>,
    prefix: String,
    depth: usize,
}

struct Walk<'s> {
    src: &'s [u8],
    out: FileExtract,
    calls: Vec<Call>,
    full: bool,
}

fn span(n: Node<'_>) -> Span {
    let s = n.start_position();
    let e = n.end_position();
    Span {
        start_line: s.row as u32 + 1,
        start_column: s.column as u32 + 1,
        end_line: e.row as u32 + 1,
        end_column: e.column as u32 + 1,
        start_byte: n.start_byte() as u32,
        end_byte: n.end_byte() as u32,
    }
}
fn props(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

impl<'s> Walk<'s> {
    fn new(path: &str, src: &'s [u8], root: Node<'_>, language: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let mut out = FileExtract::default();
        out.nodes.push(ExtractedNode {
            kind: "File".into(),
            name,
            qualified_name: path.to_owned(),
            span: span(root),
            parent: None,
            properties: props(&[("language", json!(language))]),
        });
        out.parse_errors = count_errors(root);
        Self {
            src,
            out,
            calls: Vec::new(),
            full: false,
        }
    }
    fn text(&self, n: Node<'_>) -> String {
        String::from_utf8_lossy(&self.src[n.start_byte()..n.end_byte().min(self.src.len())])
            .into_owned()
    }
    fn short(&self, n: Node<'_>) -> String {
        let t = self.text(n);
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        t.chars().take(256).collect()
    }
    fn node(
        &mut self,
        kind: &str,
        name: &str,
        qualified: &str,
        n: Node<'_>,
        parent: usize,
        properties: Map<String, Value>,
    ) -> Option<usize> {
        if self.out.nodes.len() >= MAX_NODES {
            self.full = true;
            return None;
        }
        self.out.nodes.push(ExtractedNode {
            kind: kind.into(),
            name: name.chars().take(512).collect(),
            qualified_name: qualified.chars().take(2048).collect(),
            span: span(n),
            parent: Some(parent),
            properties,
        });
        Some(self.out.nodes.len() - 1)
    }
    fn edge(
        &mut self,
        kind: &str,
        source: usize,
        target: EdgeTarget,
        resolution: Resolution,
        confidence: f32,
        properties: Map<String, Value>,
    ) {
        if self.out.edges.len() >= MAX_EDGES {
            self.full = true;
            return;
        }
        self.out.edges.push(ExtractedEdge {
            kind: kind.into(),
            source,
            target,
            resolution,
            confidence,
            properties,
        });
    }
    fn contain(&mut self, from: usize, to: usize, method: bool) {
        self.edge(
            if method { "DEFINES_METHOD" } else { "DEFINES" },
            from,
            EdgeTarget::Local(to),
            Resolution::Resolved,
            1.0,
            Map::new(),
        );
    }
    fn symbol(
        &mut self,
        kind: &str,
        from: usize,
        name: &str,
        confidence: f32,
        properties: Map<String, Value>,
    ) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.edge(
            kind,
            from,
            EdgeTarget::Symbol(name.to_owned()),
            Resolution::Unresolved,
            confidence,
            properties,
        );
    }
    fn local_named(&self, name: &str, kinds: &[&str]) -> Vec<usize> {
        self.out
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.name == name && kinds.contains(&n.kind.as_str()))
            .map(|(i, _)| i)
            .collect()
    }
    /// Resolves recorded calls against this file's definitions.
    fn finish(mut self) -> FileExtract {
        let calls = std::mem::take(&mut self.calls);
        for c in calls {
            let methods = ["Function", "Method", "Constructor"];
            let local: Vec<usize> = match (c.receiver, c.class) {
                (Receiver::SelfRef, Some(class)) => self
                    .local_named(&c.name, &methods)
                    .into_iter()
                    .filter(|i| self.out.nodes[*i].parent == Some(class))
                    .collect(),
                (Receiver::None, _) => self
                    .local_named(&c.name, &methods)
                    .into_iter()
                    .filter(|i| {
                        // A bare call names a free function or a member of the same class.
                        let p = self.out.nodes[*i].parent;
                        p == Some(0)
                            || p == c.class
                            || self.out.nodes[p.unwrap_or(0)].kind == "Function"
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let confidence = match c.receiver {
                Receiver::None => 0.7,
                Receiver::SelfRef => 0.65,
                Receiver::Other => 0.45,
            };
            let receiver = match c.receiver {
                Receiver::None => "none",
                Receiver::SelfRef => "self",
                Receiver::Other => "other",
            };
            if local.len() == 1 {
                self.edge(
                    "CALLS",
                    c.caller,
                    EdgeTarget::Local(local[0]),
                    Resolution::Inferred,
                    confidence,
                    props(&[("receiver", json!(receiver))]),
                );
            } else {
                let target = c.name;
                let mut p = props(&[("receiver", json!(receiver))]);
                if local.len() > 1 {
                    p.insert("candidates".into(), json!(local.len()));
                }
                self.symbol("CALLS", c.caller, &target, confidence, p);
            }
        }
        if self.full {
            self.out.parse_errors += 1;
            self.out.nodes[0]
                .properties
                .insert("truncated".into(), json!(true));
        }
        self.out
    }

    fn walk(
        &mut self,
        root: Node<'_>,
        lang: &str,
        stop: &(dyn Fn() -> bool + Sync),
        deadline: Instant,
    ) -> Result<(), ExtractFault> {
        let mut stack: Vec<(Node<'_>, Ctx)> = vec![(
            root,
            Ctx {
                container: 0,
                class: None,
                function: None,
                prefix: String::new(),
                depth: 0,
            },
        )];
        let mut visited = 0usize;
        while let Some((n, ctx)) = stack.pop() {
            visited += 1;
            if visited.is_multiple_of(4096) {
                if stop() {
                    return Err(ExtractFault::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(ExtractFault::TimedOut);
                }
            }
            let child_ctx = self.visit(n, lang, &ctx).unwrap_or_else(|| ctx.clone());
            if child_ctx.depth >= MAX_DEPTH || self.full {
                self.full = true;
                continue;
            }
            let mut cursor = n.walk();
            let children: Vec<Node<'_>> = n.named_children(&mut cursor).collect();
            for c in children.into_iter().rev() {
                stack.push((
                    c,
                    Ctx {
                        depth: child_ctx.depth + 1,
                        ..child_ctx.clone()
                    },
                ));
            }
        }
        Ok(())
    }

    /// Records what `n` defines or references; returns the context of its children when it
    /// opens a new container.
    fn visit(&mut self, n: Node<'_>, lang: &str, ctx: &Ctx) -> Option<Ctx> {
        let kind = n.kind();
        if let Some((label, name)) = self.class_like(n, lang) {
            let qualified = format!("{}{name}", ctx.prefix);
            let idx = self.node(label, &name, &qualified, n, ctx.container, Map::new())?;
            self.contain(ctx.container, idx, false);
            self.bases(n, lang, idx, label);
            return Some(Ctx {
                container: idx,
                class: if label == "Namespace" {
                    ctx.class
                } else {
                    Some(idx)
                },
                function: None,
                prefix: format!("{qualified}."),
                depth: ctx.depth,
            });
        }
        if let Some(name) = self.function_like(n, lang) {
            let in_class = ctx.class.is_some();
            let label = if kind == "constructor_declaration" {
                "Constructor"
            } else if kind == "property_declaration" {
                "Property"
            } else if in_class {
                "Method"
            } else {
                "Function"
            };
            let qualified = format!("{}{name}", ctx.prefix);
            let mut properties = Map::new();
            for (field, key) in [
                ("return_type", "return_type"),
                ("type", "return_type"),
                ("returns", "return_type"),
                ("parameters", "parameters"),
            ] {
                if let Some(c) = n.child_by_field_name(field)
                    && !properties.contains_key(key)
                {
                    properties.insert(key.into(), json!(self.short(c)));
                }
            }
            let idx = self.node(label, &name, &qualified, n, ctx.container, properties)?;
            self.contain(ctx.container, idx, in_class);
            return Some(Ctx {
                container: idx,
                class: ctx.class,
                function: Some(idx),
                prefix: format!("{qualified}."),
                depth: ctx.depth,
            });
        }
        for module in self.imports(n, lang) {
            self.symbol("IMPORTS", 0, &module, 1.0, Map::new());
        }
        if let Some((name, receiver)) = self.call(n, lang) {
            if lang != "csharp" && name == "require" && receiver == Receiver::None {
                if let Some(arg) = first_string(self, n) {
                    self.symbol("IMPORTS", 0, &arg, 1.0, Map::new());
                }
                return None;
            }
            self.calls.push(Call {
                caller: ctx.function.unwrap_or(ctx.container),
                class: ctx.class,
                name,
                receiver,
            });
        }
        None
    }

    fn class_like(&self, n: Node<'_>, lang: &str) -> Option<(&'static str, String)> {
        let label = match (lang, n.kind()) {
            ("csharp", "class_declaration") => "Class",
            ("csharp", "interface_declaration") => "Interface",
            ("csharp", "struct_declaration") => "Struct",
            ("csharp", "enum_declaration") => "Enum",
            ("csharp", "record_declaration") => "Record",
            ("csharp", "namespace_declaration" | "file_scoped_namespace_declaration") => {
                "Namespace"
            }
            (
                "javascript" | "typescript" | "tsx",
                "class_declaration" | "class" | "abstract_class_declaration",
            ) => "Class",
            ("typescript" | "tsx", "interface_declaration") => "Interface",
            ("typescript" | "tsx", "enum_declaration") => "Enum",
            ("typescript" | "tsx", "internal_module" | "module") => "Namespace",
            _ => return None,
        };
        let name = n.child_by_field_name("name")?;
        Some((label, self.text(name)))
    }

    fn function_like(&self, n: Node<'_>, lang: &str) -> Option<String> {
        match (lang, n.kind()) {
            (
                "csharp",
                "method_declaration"
                | "constructor_declaration"
                | "local_function_statement"
                | "property_declaration",
            )
            | (
                "javascript" | "typescript" | "tsx",
                "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "method_signature"
                | "abstract_method_signature"
                | "function_signature",
            ) => Some(self.text(n.child_by_field_name("name")?)),
            ("javascript" | "typescript" | "tsx", "variable_declarator") => {
                let v = n.child_by_field_name("value")?;
                if !matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                ) {
                    return None;
                }
                let name = n.child_by_field_name("name")?;
                if name.kind() != "identifier" {
                    return None;
                }
                Some(self.text(name))
            }
            _ => None,
        }
    }

    fn bases(&mut self, n: Node<'_>, lang: &str, idx: usize, label: &str) {
        let mut out: Vec<(String, &'static str, bool)> = Vec::new();
        match lang {
            "csharp" => {
                // C# syntax does not say which base is the class: the first is INHERITS when the
                // name does not follow the interface convention, others IMPLEMENTS (inferred).
                if let Some(b) = child_of_kind(n, "base_list") {
                    let mut c = b.walk();
                    for (i, t) in b.named_children(&mut c).enumerate() {
                        let name = self.text(t);
                        let interface_like = name.len() > 1
                            && name.starts_with('I')
                            && name.chars().nth(1).is_some_and(char::is_uppercase);
                        let edge = if label == "Interface" || interface_like || i > 0 {
                            "IMPLEMENTS"
                        } else {
                            "INHERITS"
                        };
                        out.push((name, edge, true));
                    }
                }
            }
            "javascript" | "typescript" | "tsx" => {
                if let Some(h) = child_of_kind(n, "class_heritage") {
                    match child_of_kind(h, "extends_clause") {
                        Some(e) => {
                            if let Some(v) = e.child_by_field_name("value") {
                                out.push((self.text(v), "INHERITS", false));
                            }
                        }
                        // JavaScript: `class A extends B` puts the expression directly.
                        None => {
                            let mut c = h.walk();
                            if let Some(v) = h
                                .named_children(&mut c)
                                .find(|x| x.kind() != "implements_clause")
                            {
                                out.push((self.text(v), "INHERITS", false));
                            }
                        }
                    }
                    if let Some(i) = child_of_kind(h, "implements_clause") {
                        for t in descendants_of(i, &["type_identifier"]) {
                            out.push((self.text(t), "IMPLEMENTS", false));
                        }
                    }
                }
                if let Some(e) = child_of_kind(n, "extends_type_clause") {
                    for t in descendants_of(e, &["type_identifier"]) {
                        out.push((self.text(t), "INHERITS", false));
                    }
                }
            }
            _ => {}
        }
        for (name, edge, inferred) in out {
            let mut p = Map::new();
            if inferred {
                p.insert("inheritance_inferred".into(), json!(true));
            }
            self.symbol(edge, idx, &name, if inferred { 0.6 } else { 0.9 }, p);
        }
    }

    fn imports(&self, n: Node<'_>, lang: &str) -> Vec<String> {
        let strip = |s: String| s.trim_matches(['"', '\'', '`', '<', '>']).to_owned();
        match (lang, n.kind()) {
            ("csharp", "using_directive") => {
                let mut c = n.walk();
                n.named_children(&mut c)
                    .filter(|x| matches!(x.kind(), "qualified_name" | "identifier"))
                    .last()
                    .map(|x| vec![self.text(x)])
                    .unwrap_or_default()
            }
            ("javascript" | "typescript" | "tsx", "import_statement" | "export_statement") => n
                .child_by_field_name("source")
                .map(|s| vec![strip(self.text(s))])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn call(&self, n: Node<'_>, lang: &str) -> Option<(String, Receiver)> {
        let receiver_of = |obj: Option<Node<'_>>| match obj.map(|o| o.kind()) {
            None => Receiver::None,
            Some("this" | "this_expression") => Receiver::SelfRef,
            Some(_) => {
                if obj.map(|o| self.text(o)).as_deref() == Some("this") {
                    Receiver::SelfRef
                } else {
                    Receiver::Other
                }
            }
        };
        match (lang, n.kind()) {
            ("javascript" | "typescript" | "tsx", "call_expression") => {
                let f = n.child_by_field_name("function")?;
                match f.kind() {
                    "identifier" => Some((self.text(f), Receiver::None)),
                    "member_expression" => {
                        let p = f.child_by_field_name("property")?;
                        Some((self.text(p), receiver_of(f.child_by_field_name("object"))))
                    }
                    _ => None,
                }
            }
            ("csharp", "invocation_expression") => {
                let f = n.child_by_field_name("function")?;
                match f.kind() {
                    "identifier" => Some((self.text(f), Receiver::None)),
                    "member_access_expression" => {
                        let name = f.child_by_field_name("name")?;
                        Some((
                            self.text(name),
                            receiver_of(f.child_by_field_name("expression")),
                        ))
                    }
                    "generic_name" => Some((self.text(f.named_child(0)?), Receiver::None)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn count_errors(root: Node<'_>) -> u32 {
    if !root.has_error() {
        return 0;
    }
    let mut n = 0u32;
    let mut stack = vec![root];
    while let Some(x) = stack.pop() {
        if x.is_error() || x.is_missing() {
            n += 1;
            continue;
        }
        if x.has_error() {
            let mut c = x.walk();
            stack.extend(x.children(&mut c));
        }
        if n > 10_000 {
            break;
        }
    }
    n.max(1)
}
fn child_of_kind<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut c = n.walk();
    n.named_children(&mut c).find(|x| x.kind() == kind)
}
fn descendants_of<'t>(n: Node<'t>, kinds: &[&str]) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        if kinds.contains(&x.kind()) {
            out.push(x);
            continue;
        }
        let mut c = x.walk();
        let children: Vec<Node<'t>> = x.named_children(&mut c).collect();
        stack.extend(children.into_iter().rev());
        if out.len() > 256 {
            break;
        }
    }
    out
}
fn first_string(w: &Walk<'_>, n: Node<'_>) -> Option<String> {
    let args = n.child_by_field_name("arguments")?;
    descendants_of(args, &["string_fragment", "string_content"])
        .first()
        .map(|s| w.text(*s))
}

// ---------------------------------------------------------------------------------------------
// Domain extractors: SQL, Terraform, Ansible and ColdFusion are ports of the retained CodeGraph
// extractors in `super::domain` (dispatched from `extract`).
