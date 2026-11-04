//! Tree-sitter structural extraction (CODE-01/02), retaining the CodeGraph `LanguageSpecs`
//! surface (C, C++, Python, Go, Java, Ruby, Rust, PHP, Bash, Swift) and the frozen adapter
//! languages with maintained Rust grammar crates (C#, JavaScript/JSX, TypeScript/TSX, SQL,
//! Terraform/HCL, Ansible/YAML, ColdFusion). Each extractor reports what it supports in its
//! [`LanguageInfo::capabilities`]; nothing here claims compiler semantics.
//!
//! Output per file: a `File` node (index 0), definitions with 1-based spans and qualified names
//! (`Outer.Inner.member`, nested by syntax), `DEFINES`/`DEFINES_METHOD` containment, `IMPORTS`
//! to the imported module name, `CALLS` from the enclosing callable, and inheritance
//! (`INHERITS`/`IMPLEMENTS`). A relationship whose target is a unique definition of the same
//! file is `Inferred` locally; any other is a symbol for the indexer to link or leave
//! `Unresolved`. Call confidences are the retained values: 0.7 for a bare call, 0.65 for a call
//! on `self`/`this`, 0.45 for a call on another receiver. Inheritance chosen by a naming
//! heuristic (a Swift class's first entry) is marked `inheritance_inferred`.
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

/// Most nodes and edges kept per file (beyond them the file is recorded partial).
const MAX_NODES: usize = 10_000;
const MAX_EDGES: usize = 40_000;
/// Deepest syntax nesting walked.
const MAX_DEPTH: usize = 2_000;

const DEFS_IMPORTS: &[&str] = &["definitions", "spans", "containment", "imports"];
const FULL: &[&str] = &["definitions", "spans", "containment", "imports", "calls"];
const FULL_INHERIT: &[&str] = &[
    "definitions",
    "spans",
    "containment",
    "imports",
    "calls",
    "inheritance",
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

/// Every language this build knows.
pub fn languages() -> Vec<LanguageInfo> {
    vec![
        lang!("c", "C", &[".c", ".h"], "tree-sitter-c 0.24.2", FULL),
        lang!(
            "cpp",
            "C++",
            &[".cpp", ".cc", ".cxx", ".hpp", ".hxx", ".hh"],
            "tree-sitter-cpp 0.23.4",
            FULL_INHERIT
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
            FULL_INHERIT
        ),
        lang!("go", "Go", &[".go"], "tree-sitter-go 0.25.0", FULL),
        lang!(
            "java",
            "Java",
            &[".java"],
            "tree-sitter-java 0.23.5",
            FULL_INHERIT
        ),
        lang!(
            "ruby",
            "Ruby",
            &[".rb"],
            "tree-sitter-ruby 0.23.1",
            FULL_INHERIT
        ),
        lang!(
            "rust",
            "Rust",
            &[".rs"],
            "tree-sitter-rust 0.24.2",
            FULL_INHERIT
        ),
        lang!(
            "php",
            "PHP",
            &[".php"],
            "tree-sitter-php 0.25.1",
            FULL_INHERIT
        ),
        lang!(
            "bash",
            "Bash",
            &[".sh", ".bash", ".zsh"],
            "tree-sitter-bash 0.25.1",
            FULL
        ),
        lang!(
            "swift",
            "Swift",
            &[".swift"],
            "tree-sitter-swift 0.7.4",
            FULL_INHERIT
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
            &[
                "definitions",
                "spans",
                "containment",
                "columns",
                "foreign-keys",
                "view-queries"
            ]
        ),
        lang!(
            "terraform",
            "Terraform/HCL",
            &[".tf", ".tfvars", ".hcl"],
            "tree-sitter-hcl 1.1.0",
            &["definitions", "spans", "modules", "references"]
        ),
        lang!(
            "yaml",
            "YAML/Ansible",
            &[".yml", ".yaml"],
            "tree-sitter-yaml 0.7.2",
            &["syntax", "ansible-plays", "ansible-tasks", "ansible-roles"]
        ),
        lang!(
            "coldfusion",
            "ColdFusion",
            &[".cfm", ".cfc", ".cfml"],
            "tree-sitter-cfml 0.26.44",
            DEFS_IMPORTS
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
        for (i, l) in languages.iter().enumerate() {
            for e in l.extensions {
                by_extension.insert(*e, i);
            }
        }
        Self {
            languages,
            by_extension,
        }
    }
}

impl StructuralParser for TreeSitterParser {
    fn languages(&self) -> &[LanguageInfo] {
        &self.languages
    }
    fn language_of(&self, path: &str) -> Option<&LanguageInfo> {
        let name = path.rsplit('/').next().unwrap_or(path);
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
        source: &[u8],
        stop: &(dyn Fn() -> bool + Sync),
        budget_ms: u64,
    ) -> Result<FileExtract, ExtractFault> {
        if source.iter().take(8192).any(|b| *b == 0) {
            return Err(ExtractFault::Failed("binary content".into()));
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
            "sql" => sql(&mut w, root),
            "terraform" => hcl(&mut w, root),
            "yaml" => yaml(&mut w, root),
            "coldfusion" | "cfscript" => cfml(&mut w, root),
            _ => w.walk(root, grammar_id, stop, deadline)?,
        }
        if stop() {
            return Err(ExtractFault::Cancelled);
        }
        Ok(w.finish())
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
    qualified: Option<String>,
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
                let target = c.qualified.unwrap_or(c.name);
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
            let mut properties = Map::new();
            if lang == "swift"
                && let Some(k) = field_text(self, n, "declaration_kind")
            {
                properties.insert("declaration_kind".into(), json!(k));
            }
            let idx = self.node(label, &name, &qualified, n, ctx.container, properties)?;
            self.contain(ctx.container, idx, false);
            self.bases(n, lang, idx, label);
            return Some(Ctx {
                container: idx,
                class: if matches!(label, "Namespace" | "Module") {
                    ctx.class
                } else {
                    Some(idx)
                },
                function: None,
                prefix: format!("{qualified}."),
                depth: ctx.depth,
            });
        }
        if let Some(owner) = self.container_only(n, lang) {
            // `impl Type`, `extension Type`: members qualify with the type; the type node is
            // the local definition of that name, when the file has one.
            let local =
                self.local_named(&owner, &["Class", "Struct", "Enum", "Interface", "Record"]);
            let class = (local.len() == 1).then(|| local[0]);
            if let (Some(c), Some(t)) = (class, self.implemented_trait(n, lang)) {
                self.symbol("IMPLEMENTS", c, &t, 0.9, Map::new());
            }
            return Some(Ctx {
                container: class.unwrap_or(ctx.container),
                class: class.or(ctx.class),
                function: None,
                prefix: format!("{}{owner}.", ctx.prefix),
                depth: ctx.depth,
            });
        }
        if let Some((name, receiver)) = self.function_like(n, lang) {
            let in_class = ctx.class.is_some() || receiver.is_some();
            let label = if is_constructor(kind) {
                "Constructor"
            } else if kind == "property_declaration" || kind == "protocol_property_declaration" {
                "Property"
            } else if in_class || matches!(lang, "java" | "ruby") {
                "Method"
            } else {
                "Function"
            };
            let prefix = match &receiver {
                Some(r) => format!("{}{r}.", ctx.prefix),
                None => ctx.prefix.clone(),
            };
            let qualified = format!("{prefix}{name}");
            let mut properties = Map::new();
            for (field, key) in [
                ("return_type", "return_type"),
                ("result", "return_type"),
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
            if let Some(abi) = self.abi(n, lang) {
                properties.insert("abi".into(), json!(abi));
            }
            let class = match &receiver {
                Some(r) => {
                    let local = self.local_named(r, &["Class", "Struct", "Interface"]);
                    (local.len() == 1).then(|| local[0]).or(ctx.class)
                }
                None => ctx.class,
            };
            let parent = class
                .filter(|_| receiver.is_some())
                .unwrap_or(ctx.container);
            let idx = self.node(label, &name, &qualified, n, parent, properties)?;
            self.contain(parent, idx, in_class);
            return Some(Ctx {
                container: idx,
                class,
                function: Some(idx),
                prefix: format!("{qualified}."),
                depth: ctx.depth,
            });
        }
        for module in self.imports(n, lang) {
            self.symbol("IMPORTS", 0, &module, 1.0, Map::new());
        }
        if let Some((name, receiver, qualified)) = self.call(n, lang) {
            if lang == "ruby" && matches!(name.as_str(), "require" | "require_relative" | "load") {
                if let Some(arg) = first_string(self, n) {
                    self.symbol("IMPORTS", 0, &arg, 1.0, Map::new());
                }
                return None;
            }
            if matches!(lang, "javascript" | "typescript" | "tsx")
                && name == "require"
                && receiver == Receiver::None
            {
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
                qualified,
            });
        }
        None
    }

    fn class_like(&self, n: Node<'_>, lang: &str) -> Option<(&'static str, String)> {
        let kind = n.kind();
        let label = match (lang, kind) {
            ("c" | "cpp", "struct_specifier") | ("c" | "cpp", "union_specifier") => {
                n.child_by_field_name("body")?;
                "Struct"
            }
            ("c" | "cpp", "enum_specifier") => {
                n.child_by_field_name("body")?;
                "Enum"
            }
            ("c" | "cpp", "type_definition") => {
                let d = n.child_by_field_name("declarator")?;
                return Some(("Struct", self.text(d)));
            }
            ("cpp", "class_specifier") => {
                n.child_by_field_name("body")?;
                "Class"
            }
            ("cpp", "namespace_definition") => "Namespace",
            ("csharp", "class_declaration") => "Class",
            ("csharp", "interface_declaration") => "Interface",
            ("csharp", "struct_declaration") => "Struct",
            ("csharp", "enum_declaration") => "Enum",
            ("csharp", "record_declaration") => "Record",
            ("csharp", "namespace_declaration" | "file_scoped_namespace_declaration") => {
                "Namespace"
            }
            ("python", "class_definition") => "Class",
            ("go", "type_spec") => match n.child_by_field_name("type").map(|t| t.kind()) {
                Some("struct_type") => "Struct",
                Some("interface_type") => "Interface",
                _ => "Class",
            },
            ("java", "class_declaration") => "Class",
            ("java", "interface_declaration") => "Interface",
            ("java", "enum_declaration") => "Enum",
            ("java", "record_declaration") => "Record",
            ("ruby", "class") => "Class",
            ("ruby", "module") => "Module",
            ("rust", "struct_item") => "Struct",
            ("rust", "enum_item") => "Enum",
            ("rust", "trait_item") => "Interface",
            ("rust", "mod_item") => {
                n.child_by_field_name("body")?;
                "Module"
            }
            ("php", "class_declaration") => "Class",
            ("php", "interface_declaration") => "Interface",
            ("php", "trait_declaration") => "Interface",
            ("swift", "class_declaration") => {
                match field_text(self, n, "declaration_kind").as_deref() {
                    Some("extension") => return None,
                    Some("struct") => "Struct",
                    Some("enum") => "Enum",
                    _ => "Class",
                }
            }
            ("swift", "protocol_declaration") => "Interface",
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
        let text = if lang == "swift" && name.kind() != "type_identifier" {
            last_descendant(self, name, "type_identifier").unwrap_or_else(|| self.text(name))
        } else {
            self.text(name)
        };
        Some((label, text))
    }

    fn container_only(&self, n: Node<'_>, lang: &str) -> Option<String> {
        match (lang, n.kind()) {
            ("rust", "impl_item") => nominal(self, n.child_by_field_name("type")?),
            ("swift", "class_declaration")
                if field_text(self, n, "declaration_kind").as_deref() == Some("extension") =>
            {
                let name = n.child_by_field_name("name")?;
                last_descendant(self, name, "type_identifier").or_else(|| Some(self.text(name)))
            }
            _ => None,
        }
    }
    fn implemented_trait(&self, n: Node<'_>, lang: &str) -> Option<String> {
        match lang {
            "rust" => n
                .child_by_field_name("trait")
                .and_then(|t| nominal(self, t)),
            _ => None,
        }
    }

    fn function_like(&self, n: Node<'_>, lang: &str) -> Option<(String, Option<String>)> {
        let kind = n.kind();
        let name = match (lang, kind) {
            ("c" | "cpp", "function_definition") => {
                let d = n.child_by_field_name("declarator")?;
                return c_function_name(self, d);
            }
            (
                "csharp",
                "method_declaration"
                | "constructor_declaration"
                | "local_function_statement"
                | "property_declaration",
            ) => self.text(n.child_by_field_name("name")?),
            ("python", "function_definition") => self.text(n.child_by_field_name("name")?),
            ("go", "function_declaration") => self.text(n.child_by_field_name("name")?),
            ("go", "method_declaration") => {
                let name = self.text(n.child_by_field_name("name")?);
                let receiver = n
                    .child_by_field_name("receiver")
                    .and_then(|r| last_descendant(self, r, "type_identifier"));
                return Some((name, receiver));
            }
            ("java", "method_declaration" | "constructor_declaration") => {
                self.text(n.child_by_field_name("name")?)
            }
            ("ruby", "method" | "singleton_method") => self.text(n.child_by_field_name("name")?),
            ("rust", "function_item" | "function_signature_item") => {
                self.text(n.child_by_field_name("name")?)
            }
            ("php", "function_definition" | "method_declaration") => {
                self.text(n.child_by_field_name("name")?)
            }
            ("bash", "function_definition") => self.text(n.child_by_field_name("name")?),
            ("swift", "function_declaration" | "protocol_function_declaration") => {
                self.text(n.child_by_field_name("name")?)
            }
            ("swift", "init_declaration") => "init".into(),
            ("swift", "deinit_declaration") => "deinit".into(),
            ("swift", "subscript_declaration") => "subscript".into(),
            (
                "javascript" | "typescript" | "tsx",
                "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "method_signature"
                | "abstract_method_signature"
                | "function_signature",
            ) => self.text(n.child_by_field_name("name")?),
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
                self.text(name)
            }
            _ => return None,
        };
        Some((name, None))
    }

    fn bases(&mut self, n: Node<'_>, lang: &str, idx: usize, label: &str) {
        let mut out: Vec<(String, &'static str, bool)> = Vec::new();
        match lang {
            "python" => {
                if let Some(s) = n.child_by_field_name("superclasses") {
                    let mut c = s.walk();
                    for b in s.named_children(&mut c) {
                        if matches!(b.kind(), "identifier" | "attribute") {
                            out.push((self.text(b), "INHERITS", false));
                        }
                    }
                }
            }
            "java" => {
                if let Some(s) = n.child_by_field_name("superclass") {
                    for t in descendants_of(s, &["type_identifier", "scoped_type_identifier"]) {
                        out.push((self.text(t), "INHERITS", false));
                    }
                }
                for field in ["interfaces", "extends"] {
                    if let Some(s) = n
                        .child_by_field_name(field)
                        .or_else(|| child_of_kind(n, "extends_interfaces"))
                    {
                        let edge = if label == "Interface" {
                            "INHERITS"
                        } else {
                            "IMPLEMENTS"
                        };
                        for t in descendants_of(s, &["type_identifier"]) {
                            out.push((self.text(t), edge, false));
                        }
                    }
                }
            }
            "ruby" => {
                if let Some(s) = n.child_by_field_name("superclass") {
                    out.push((
                        self.text(s).trim_start_matches('<').trim().to_owned(),
                        "INHERITS",
                        false,
                    ));
                }
            }
            "cpp" => {
                if let Some(b) = child_of_kind(n, "base_class_clause") {
                    for t in descendants_of(b, &["type_identifier", "qualified_identifier"]) {
                        out.push((self.text(t), "INHERITS", false));
                    }
                }
            }
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
            "php" => {
                if let Some(b) = child_of_kind(n, "base_clause") {
                    for t in descendants_of(b, &["name", "qualified_name"]) {
                        out.push((self.text(t), "INHERITS", false));
                    }
                }
                if let Some(b) = child_of_kind(n, "class_interface_clause") {
                    for t in descendants_of(b, &["name", "qualified_name"]) {
                        out.push((self.text(t), "IMPLEMENTS", false));
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
            "swift" => {
                // The retained Swift rules: protocols inherit; struct/enum/actor entries are
                // conformances; a class's first entry is a superclass unless it looks like a
                // protocol (inferred either way), later entries conform.
                let mut c = n.walk();
                let entries: Vec<String> = n
                    .named_children(&mut c)
                    .filter(|x| x.kind() == "inheritance_specifier")
                    .filter_map(|x| x.child_by_field_name("inherits_from").map(|t| self.text(t)))
                    .collect();
                let decl = field_text(self, n, "declaration_kind").unwrap_or_default();
                for (i, e) in entries.into_iter().enumerate() {
                    if label == "Interface" {
                        if e != "AnyObject" {
                            out.push((e, "INHERITS", false));
                        }
                    } else if decl == "class" && i == 0 {
                        // A type of this file decides; otherwise the naming heuristic (inferred).
                        let simple = e.rsplit('.').next().unwrap_or(&e).to_owned();
                        if !self.local_named(&simple, &["Interface"]).is_empty() {
                            out.push((e, "IMPLEMENTS", false));
                        } else if !self.local_named(&simple, &["Class"]).is_empty() {
                            out.push((e, "INHERITS", false));
                        } else {
                            let protocol = swift_protocol_like(&e);
                            out.push((e, if protocol { "IMPLEMENTS" } else { "INHERITS" }, true));
                        }
                    } else {
                        out.push((e, "IMPLEMENTS", false));
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
            ("c" | "cpp", "preproc_include") => n
                .child_by_field_name("path")
                .map(|p| vec![strip(self.text(p))])
                .unwrap_or_default(),
            ("csharp", "using_directive") => {
                let mut c = n.walk();
                n.named_children(&mut c)
                    .filter(|x| matches!(x.kind(), "qualified_name" | "identifier"))
                    .last()
                    .map(|x| vec![self.text(x)])
                    .unwrap_or_default()
            }
            ("python", "import_statement") => {
                let mut out = Vec::new();
                let mut c = n.walk();
                for x in n.named_children(&mut c) {
                    match x.kind() {
                        "dotted_name" => out.push(self.text(x)),
                        "aliased_import" => {
                            if let Some(nm) = x.child_by_field_name("name") {
                                out.push(self.text(nm));
                            }
                        }
                        _ => {}
                    }
                }
                out
            }
            ("python", "import_from_statement") => n
                .child_by_field_name("module_name")
                .map(|m| vec![self.text(m)])
                .unwrap_or_default(),
            ("go", "import_spec") => n
                .child_by_field_name("path")
                .map(|p| vec![strip(self.text(p))])
                .unwrap_or_default(),
            ("java", "import_declaration") => {
                let mut c = n.walk();
                n.named_children(&mut c)
                    .find(|x| matches!(x.kind(), "scoped_identifier" | "identifier"))
                    .map(|x| vec![self.text(x)])
                    .unwrap_or_default()
            }
            ("rust", "use_declaration") => n
                .child_by_field_name("argument")
                .map(|a| vec![self.short(a)])
                .unwrap_or_default(),
            ("php", "namespace_use_clause") => vec![self.short(n)],
            ("bash", "command") => {
                let name = n
                    .child_by_field_name("name")
                    .map(|x| self.text(x))
                    .unwrap_or_default();
                if name == "source" || name == "." {
                    n.child_by_field_name("argument")
                        .map(|a| vec![strip(self.text(a))])
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
            ("swift", "import_declaration") => {
                let mut c = n.walk();
                n.named_children(&mut c)
                    .find(|x| x.kind() == "identifier")
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

    fn call(&self, n: Node<'_>, lang: &str) -> Option<(String, Receiver, Option<String>)> {
        let receiver_of = |obj: Option<Node<'_>>| match obj.map(|o| o.kind()) {
            None => Receiver::None,
            Some("self" | "this" | "self_expression" | "this_expression") => Receiver::SelfRef,
            Some(_) => {
                let t = obj.map(|o| self.text(o)).unwrap_or_default();
                if t == "self" || t == "this" || t == "$this" {
                    Receiver::SelfRef
                } else {
                    Receiver::Other
                }
            }
        };
        match (lang, n.kind()) {
            (
                "c" | "cpp" | "rust" | "go" | "javascript" | "typescript" | "tsx" | "swift",
                "call_expression",
            ) => {
                let f = n
                    .child_by_field_name("function")
                    .or_else(|| n.named_child(0))?;
                match f.kind() {
                    "identifier" | "simple_identifier" => {
                        Some((self.text(f), Receiver::None, None))
                    }
                    "field_expression" => {
                        let field = f.child_by_field_name("field")?;
                        let obj = f
                            .child_by_field_name("argument")
                            .or_else(|| f.child_by_field_name("value"));
                        Some((self.text(field), receiver_of(obj), None))
                    }
                    "selector_expression" => {
                        let field = f.child_by_field_name("field")?;
                        Some((
                            self.text(field),
                            receiver_of(f.child_by_field_name("operand")),
                            None,
                        ))
                    }
                    "member_expression" => {
                        let p = f.child_by_field_name("property")?;
                        Some((
                            self.text(p),
                            receiver_of(f.child_by_field_name("object")),
                            None,
                        ))
                    }
                    "scoped_identifier" | "qualified_identifier" => {
                        let name = f.child_by_field_name("name")?;
                        Some((
                            last_identifier(self, name),
                            Receiver::None,
                            Some(self.text(f).replace("::", ".")),
                        ))
                    }
                    "navigation_expression" => {
                        let s = f.child_by_field_name("suffix")?;
                        let name = s.child_by_field_name("suffix").unwrap_or(s);
                        Some((
                            self.text(name).trim_start_matches('.').to_owned(),
                            receiver_of(f.child_by_field_name("target")),
                            None,
                        ))
                    }
                    _ => None,
                }
            }
            ("csharp", "invocation_expression") => {
                let f = n.child_by_field_name("function")?;
                match f.kind() {
                    "identifier" => Some((self.text(f), Receiver::None, None)),
                    "member_access_expression" => {
                        let name = f.child_by_field_name("name")?;
                        Some((
                            self.text(name),
                            receiver_of(f.child_by_field_name("expression")),
                            None,
                        ))
                    }
                    "generic_name" => Some((self.text(f.named_child(0)?), Receiver::None, None)),
                    _ => None,
                }
            }
            ("python", "call") => {
                let f = n.child_by_field_name("function")?;
                match f.kind() {
                    "identifier" => Some((self.text(f), Receiver::None, None)),
                    "attribute" => {
                        let a = f.child_by_field_name("attribute")?;
                        Some((
                            self.text(a),
                            receiver_of(f.child_by_field_name("object")),
                            None,
                        ))
                    }
                    _ => None,
                }
            }
            ("java", "method_invocation") => {
                let name = n.child_by_field_name("name")?;
                Some((
                    self.text(name),
                    receiver_of(n.child_by_field_name("object")),
                    None,
                ))
            }
            ("ruby", "call") => {
                let m = n.child_by_field_name("method")?;
                Some((
                    self.text(m),
                    receiver_of(n.child_by_field_name("receiver")),
                    None,
                ))
            }
            ("php", "function_call_expression") => Some((
                self.text(n.child_by_field_name("function")?),
                Receiver::None,
                None,
            )),
            ("php", "member_call_expression") => {
                let name = n.child_by_field_name("name")?;
                Some((
                    self.text(name),
                    receiver_of(n.child_by_field_name("object")),
                    None,
                ))
            }
            ("php", "scoped_call_expression") => {
                let name = n.child_by_field_name("name")?;
                let scope = n.child_by_field_name("scope").map(|s| self.text(s));
                let receiver = if matches!(scope.as_deref(), Some("self" | "static" | "parent")) {
                    Receiver::SelfRef
                } else {
                    Receiver::Other
                };
                Some((
                    self.text(name),
                    receiver,
                    scope.map(|s| format!("{s}.{}", self.text(name))),
                ))
            }
            ("bash", "command") => {
                // Only calls of functions this file defines (every other command is a program).
                let name = self.text(n.child_by_field_name("name")?);
                (!self.local_named(&name, &["Function"]).is_empty()).then_some((
                    name,
                    Receiver::None,
                    None,
                ))
            }
            _ => None,
        }
    }

    fn abi(&self, n: Node<'_>, lang: &str) -> Option<String> {
        match lang {
            "rust" => {
                let mut c = n.walk();
                let modifiers = n
                    .named_children(&mut c)
                    .find(|x| x.kind() == "function_modifiers")?;
                let mut c2 = modifiers.walk();
                let ext = modifiers
                    .named_children(&mut c2)
                    .find(|x| x.kind() == "extern_modifier")?;
                // Exported by symbol name only (`#[no_mangle]`/`#[export_name]` above it).
                let mut prev = n.prev_named_sibling();
                let mut exported = false;
                while let Some(p) = prev {
                    if p.kind() == "line_comment" || p.kind() == "block_comment" {
                        prev = p.prev_named_sibling();
                        continue;
                    }
                    if p.kind() != "attribute_item" {
                        break;
                    }
                    let t = self.text(p);
                    if t.contains("no_mangle") || t.contains("export_name") {
                        exported = true;
                    }
                    prev = p.prev_named_sibling();
                }
                if !exported {
                    return None;
                }
                let abi = self
                    .text(ext)
                    .trim_start_matches("extern")
                    .trim()
                    .trim_matches('"')
                    .to_owned();
                Some(if abi.is_empty() { "C".into() } else { abi }).filter(|a| a != "Rust")
            }
            "cpp" => {
                let mut p = n.parent()?;
                if p.kind() == "declaration_list" {
                    p = p.parent()?;
                }
                (p.kind() == "linkage_specification"
                    && p.child_by_field_name("value")
                        .is_some_and(|v| self.text(v).trim_matches('"') == "C"))
                .then(|| "C".to_owned())
            }
            _ => None,
        }
    }
}

fn is_constructor(kind: &str) -> bool {
    matches!(kind, "constructor_declaration" | "init_declaration")
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
fn field_text(w: &Walk<'_>, n: Node<'_>, field: &str) -> Option<String> {
    n.child_by_field_name(field).map(|f| w.text(f))
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
fn last_descendant(w: &Walk<'_>, n: Node<'_>, kind: &str) -> Option<String> {
    descendants_of(n, &[kind]).last().map(|x| w.text(*x))
}
fn last_identifier(w: &Walk<'_>, n: Node<'_>) -> String {
    let t = w.text(n);
    t.rsplit("::").next().unwrap_or(&t).to_owned()
}
/// The nominal type of a Rust `impl` target (`Vec<T>` → `Vec`, `a::B` → `B`).
fn nominal(w: &Walk<'_>, n: Node<'_>) -> Option<String> {
    match n.kind() {
        "type_identifier" | "identifier" => Some(w.text(n)),
        "generic_type" => nominal(w, n.child_by_field_name("type")?),
        "scoped_type_identifier" => n.child_by_field_name("name").and_then(|x| nominal(w, x)),
        "reference_type" => nominal(w, n.child_by_field_name("type")?),
        _ => descendants_of(n, &["type_identifier"])
            .first()
            .map(|x| w.text(*x)),
    }
}
/// The name of a C/C++ function declarator, with its qualifying owner (`A::m` → (`m`, `A`)).
fn c_function_name(w: &Walk<'_>, mut d: Node<'_>) -> Option<(String, Option<String>)> {
    for _ in 0..16 {
        match d.kind() {
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator" => {
                d = d
                    .child_by_field_name("declarator")
                    .or_else(|| d.named_child(0))?;
            }
            "identifier" | "field_identifier" | "operator_name" | "destructor_name" => {
                return Some((w.text(d), None));
            }
            "qualified_identifier" => {
                let text = w.text(d);
                let mut parts: Vec<&str> = text.split("::").collect();
                let name = parts.pop()?.to_owned();
                let owner = parts.last().map(|s| (*s).to_owned());
                return Some((name, owner));
            }
            _ => return None,
        }
    }
    None
}
fn first_string(w: &Walk<'_>, n: Node<'_>) -> Option<String> {
    let args = n.child_by_field_name("arguments")?;
    descendants_of(args, &["string_fragment", "string_content"])
        .first()
        .map(|s| w.text(*s))
}
const SWIFT_PROTOCOLS: &[&str] = &[
    "Actor",
    "AnyActor",
    "App",
    "AsyncIteratorProtocol",
    "AsyncSequence",
    "BidirectionalCollection",
    "CaseIterable",
    "Codable",
    "Collection",
    "Comparable",
    "CustomDebugStringConvertible",
    "CustomStringConvertible",
    "Decodable",
    "Encodable",
    "Equatable",
    "Error",
    "ExpressibleByArrayLiteral",
    "ExpressibleByDictionaryLiteral",
    "ExpressibleByIntegerLiteral",
    "ExpressibleByStringLiteral",
    "Hashable",
    "Identifiable",
    "IteratorProtocol",
    "LocalizedError",
    "MutableCollection",
    "NSCoding",
    "NSCopying",
    "NSSecureCoding",
    "ObservableObject",
    "OptionSet",
    "PreviewProvider",
    "RandomAccessCollection",
    "RawRepresentable",
    "RangeReplaceableCollection",
    "Scene",
    "Sendable",
    "Sequence",
    "SetAlgebra",
    "Strideable",
    "View",
    "ViewModifier",
];
/// The retained protocol heuristic for a Swift class's first inheritance entry.
fn swift_protocol_like(path: &str) -> bool {
    let simple = path.rsplit('.').next().unwrap_or(path);
    if simple.ends_with("DiffableDataSource") || simple.ends_with("DiffableDataSourceReference") {
        return false;
    }
    SWIFT_PROTOCOLS.contains(&simple)
        || ["Protocol", "Delegate", "DataSource"]
            .iter()
            .any(|s| simple.len() > s.len() && simple.ends_with(s))
}

// ---------------------------------------------------------------------------------------------
// Domain extractors

/// SQL: tables (with columns and foreign keys), views (with the relations they query),
/// functions and procedures.
fn sql(w: &mut Walk<'_>, root: Node<'_>) {
    for st in descendants_of(
        root,
        &[
            "create_table",
            "create_view",
            "create_function",
            "create_procedure",
            "create_materialized_view",
        ],
    ) {
        let Some(name_node) = child_of_kind(st, "object_reference") else {
            continue;
        };
        let name = w.text(name_node);
        let simple = name
            .rsplit('.')
            .next()
            .unwrap_or(&name)
            .trim_matches(['"', '`', '[', ']'])
            .to_owned();
        let label = match st.kind() {
            "create_table" => "Table",
            "create_view" | "create_materialized_view" => "View",
            "create_procedure" => "StoredProcedure",
            _ => "Function",
        };
        let Some(idx) = w.node(label, &simple, &name, st, 0, Map::new()) else {
            return;
        };
        w.contain(0, idx, false);
        if label == "Table" {
            for col in descendants_of(st, &["column_definition"]) {
                let Some(cn) = col.child_by_field_name("name") else {
                    continue;
                };
                let cname = w.text(cn);
                let mut p = Map::new();
                if let Some(t) = col.child_by_field_name("type") {
                    p.insert("type".into(), json!(w.short(t)));
                }
                if let Some(c) = w.node("Column", &cname, &format!("{name}.{cname}"), col, idx, p) {
                    w.edge(
                        "HAS_COLUMN",
                        idx,
                        EdgeTarget::Local(c),
                        Resolution::Resolved,
                        1.0,
                        Map::new(),
                    );
                }
                for r in descendants_of(col, &["object_reference"]) {
                    let target = w.text(r);
                    w.symbol(
                        "FOREIGN_KEY",
                        idx,
                        &target,
                        0.9,
                        props(&[("column", json!(cname))]),
                    );
                }
            }
        } else if label == "View" {
            for rel in descendants_of(st, &["relation"]) {
                if let Some(r) = child_of_kind(rel, "object_reference") {
                    let target = w.text(r);
                    w.symbol("QUERIES", idx, &target, 0.9, Map::new());
                }
            }
        }
    }
}

/// Terraform/HCL: resources, data sources, modules (with their source), variables and outputs,
/// and references between them.
fn hcl(w: &mut Walk<'_>, root: Node<'_>) {
    let Some(body) = child_of_kind(root, "body") else {
        return;
    };
    let mut c = body.walk();
    let blocks: Vec<Node<'_>> = body
        .named_children(&mut c)
        .filter(|b| b.kind() == "block")
        .collect();
    for b in blocks {
        let mut c2 = b.walk();
        let parts: Vec<Node<'_>> = b.named_children(&mut c2).collect();
        let Some(kind) = parts.first().map(|p| w.text(*p)) else {
            continue;
        };
        let labels: Vec<String> = parts
            .iter()
            .filter(|p| p.kind() == "string_lit" || p.kind() == "identifier")
            .skip(1)
            .map(|p| w.text(*p).trim_matches('"').to_owned())
            .collect();
        let (label, name, qualified) = match (kind.as_str(), labels.as_slice()) {
            ("resource", [t, n, ..]) => ("TerraformResource", n.clone(), format!("{t}.{n}")),
            ("data", [t, n, ..]) => ("TerraformDataSource", n.clone(), format!("data.{t}.{n}")),
            ("module", [n, ..]) => ("TerraformModule", n.clone(), format!("module.{n}")),
            ("variable", [n, ..]) => ("TerraformVariable", n.clone(), format!("var.{n}")),
            ("output", [n, ..]) => ("TerraformOutput", n.clone(), format!("output.{n}")),
            _ => continue,
        };
        let mut p = Map::new();
        if let Some(t) = labels
            .first()
            .filter(|_| matches!(kind.as_str(), "resource" | "data"))
        {
            p.insert("type".into(), json!(t));
        }
        let Some(idx) = w.node(label, &name, &qualified, b, 0, p) else {
            return;
        };
        w.contain(0, idx, false);
        for attr in descendants_of(b, &["attribute"]) {
            let mut c3 = attr.walk();
            let kids: Vec<Node<'_>> = attr.named_children(&mut c3).collect();
            if kind == "module"
                && kids.first().is_some_and(|k| w.text(*k) == "source")
                && let Some(v) = kids.get(1)
            {
                let src = w.text(*v).trim_matches('"').to_owned();
                w.symbol("INCLUDES_MODULE", idx, &src, 1.0, Map::new());
            }
        }
        for e in descendants_of(b, &["expression"]) {
            let mut c4 = e.walk();
            let kids: Vec<Node<'_>> = e.named_children(&mut c4).collect();
            if kids.first().is_some_and(|k| k.kind() == "variable_expr") {
                let root_name = w.text(kids[0]);
                let attrs: Vec<String> = kids[1..]
                    .iter()
                    .filter(|k| k.kind() == "get_attr")
                    .map(|k| w.text(*k).trim_start_matches('.').to_owned())
                    .collect();
                let target = match (root_name.as_str(), attrs.as_slice()) {
                    ("var" | "local" | "module", [n, ..]) => format!("{root_name}.{n}"),
                    ("data", [t, n, ..]) => format!("data.{t}.{n}"),
                    (t, [n, ..])
                        if !matches!(t, "count" | "each" | "self" | "path" | "terraform") =>
                    {
                        format!("{t}.{n}")
                    }
                    _ => continue,
                };
                w.symbol("DEPENDS_ON", idx, &target, 0.9, Map::new());
            }
        }
    }
}

/// YAML: Ansible plays (a sequence of mappings with `hosts`), their tasks and handlers, roles
/// they include, and task lists (role task files). Other YAML is recorded syntax-only.
fn yaml(w: &mut Walk<'_>, root: Node<'_>) {
    let Some(seq) = descendants_of(root, &["block_sequence"]).into_iter().next() else {
        return;
    };
    if seq
        .parent()
        .and_then(|p| p.parent())
        .is_none_or(|d| d.kind() != "document")
    {
        return;
    }
    let mut c = seq.walk();
    let items: Vec<Node<'_>> = seq.named_children(&mut c).collect();
    for (i, item) in items.into_iter().enumerate() {
        let Some(map) = descendants_of(item, &["block_mapping"]).into_iter().next() else {
            continue;
        };
        let pairs = mapping(w, map);
        let name = pairs
            .iter()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| scalar(w, *v));
        if pairs.iter().any(|(k, _)| k == "hosts") {
            let play = name.unwrap_or_else(|| format!("play {}", i + 1));
            let Some(idx) = w.node("Playbook", &play, &play, map, 0, Map::new()) else {
                return;
            };
            w.contain(0, idx, false);
            for (k, v) in &pairs {
                match k.as_str() {
                    "tasks" | "pre_tasks" | "post_tasks" | "handlers" => {
                        let label = if k == "handlers" {
                            "AnsibleHandler"
                        } else {
                            "AnsibleTask"
                        };
                        tasks(w, *v, idx, &play, label);
                    }
                    "roles" => {
                        for r in descendants_of(*v, &["block_sequence_item", "flow_node"]) {
                            if r.kind() != "block_sequence_item" {
                                continue;
                            }
                            let role =
                                match descendants_of(r, &["block_mapping"]).into_iter().next() {
                                    Some(m) => mapping(w, m)
                                        .into_iter()
                                        .find(|(k, _)| k == "role" || k == "name")
                                        .map(|(_, v)| scalar(w, v)),
                                    None => Some(scalar(w, r)),
                                };
                            if let Some(role) = role {
                                w.symbol("INCLUDES_ROLE", idx, &role, 1.0, Map::new());
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else if let Some(task) = name
            && pairs.len() > 1
        {
            // A task list (a role's tasks/main.yml): each named step is a task.
            if let Some(t) = w.node("AnsibleTask", &task, &task, map, 0, Map::new()) {
                w.contain(0, t, false);
            }
        }
    }
}
fn mapping<'t>(w: &Walk<'_>, map: Node<'t>) -> Vec<(String, Node<'t>)> {
    let mut c = map.walk();
    map.named_children(&mut c)
        .filter(|p| p.kind() == "block_mapping_pair")
        .filter_map(|p| {
            Some((
                scalar(w, p.child_by_field_name("key")?),
                p.child_by_field_name("value")?,
            ))
        })
        .collect()
}
fn scalar(w: &Walk<'_>, n: Node<'_>) -> String {
    w.short(n)
        .trim_start_matches("- ")
        .trim_matches(['"', '\''])
        .to_owned()
}
fn tasks(w: &mut Walk<'_>, list: Node<'_>, parent: usize, prefix: &str, label: &str) {
    let Some(seq) = descendants_of(list, &["block_sequence"]).into_iter().next() else {
        return;
    };
    let mut c = seq.walk();
    let items: Vec<Node<'_>> = seq.named_children(&mut c).collect();
    for (i, item) in items.into_iter().enumerate() {
        let Some(map) = descendants_of(item, &["block_mapping"]).into_iter().next() else {
            continue;
        };
        let pairs = mapping(w, map);
        let name = pairs
            .iter()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| scalar(w, *v))
            .unwrap_or_else(|| format!("task {}", i + 1));
        let mut p = Map::new();
        if let Some((module, _)) = pairs.iter().find(|(k, _)| {
            !matches!(
                k.as_str(),
                "name" | "when" | "tags" | "notify" | "register" | "become" | "vars"
            )
        }) {
            p.insert("module".into(), json!(module));
        }
        if let Some(t) = w.node(label, &name, &format!("{prefix}.{name}"), map, parent, p) {
            w.contain(parent, t, false);
            if let Some((_, v)) = pairs.iter().find(|(k, _)| k == "notify") {
                let target = scalar(w, *v);
                w.symbol("NOTIFIES_HANDLER", t, &target, 0.9, Map::new());
            }
        }
    }
}

/// ColdFusion: components and functions (tag syntax and CFScript), includes and calls.
fn cfml(w: &mut Walk<'_>, root: Node<'_>) {
    let file = w.out.nodes[0].name.clone();
    let stem = file
        .rsplit_once('.')
        .map_or(file.clone(), |(s, _)| s.to_owned());
    let component = descendants_of(root, &["cf_component_open_tag", "component"])
        .first()
        .copied()
        .and_then(|c| w.node("Class", &stem, &stem, c, 0, Map::new()));
    if let Some(c) = component {
        w.contain(0, c, false);
    }
    let parent = component.unwrap_or(0);
    for f in descendants_of(root, &["cf_function_tag", "function_declaration"]) {
        let name = if f.kind() == "function_declaration" {
            f.child_by_field_name("name").map(|n| w.text(n))
        } else {
            cf_attribute(w, f, "name")
        };
        let Some(name) = name else { continue };
        let label = if component.is_some() {
            "Method"
        } else {
            "Function"
        };
        let qualified = format!("{stem}.{name}");
        let mut p = Map::new();
        if let Some(a) = cf_attribute(w, f, "access") {
            p.insert("access".into(), json!(a));
        }
        if let Some(idx) = w.node(label, &name, &qualified, f, parent, p) {
            w.contain(parent, idx, component.is_some());
            for call in descendants_of(f, &["call_expression"]) {
                if let Some(func) = call
                    .child_by_field_name("function")
                    .filter(|x| x.kind() == "identifier")
                {
                    w.calls.push(Call {
                        caller: idx,
                        class: component,
                        name: w.text(func),
                        receiver: Receiver::None,
                        qualified: None,
                    });
                }
            }
        }
    }
    for inc in descendants_of(root, &["cf_include_tag"]) {
        if let Some(t) = cf_attribute(w, inc, "template") {
            w.symbol("IMPORTS", 0, &t, 1.0, Map::new());
        }
    }
}
fn cf_attribute(w: &Walk<'_>, tag: Node<'_>, name: &str) -> Option<String> {
    let mut c = tag.walk();
    for a in tag
        .named_children(&mut c)
        .filter(|x| x.kind() == "cf_attribute")
    {
        let mut c2 = a.walk();
        let kids: Vec<Node<'_>> = a.named_children(&mut c2).collect();
        if kids
            .first()
            .is_some_and(|k| w.text(*k).eq_ignore_ascii_case(name))
        {
            return kids
                .get(1)
                .map(|v| w.text(*v).trim_matches(['"', '\'']).to_owned());
        }
    }
    None
}
