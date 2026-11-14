//! Tree-sitter structural extraction (CODE-01/02), retaining the CodeGraph `LanguageSpecs`
//! surface (C, C++, Python, Go, Java, Ruby, Rust, PHP, Bash, Swift) and the frozen adapter
//! languages with maintained Rust grammar crates (C#, JavaScript/JSX, TypeScript/TSX, SQL,
//! Terraform/HCL, Ansible/YAML, ColdFusion). Each extractor reports what it supports in its
//! [`LanguageInfo::capabilities`]; nothing here claims compiler semantics.
//!
//! Every grammar language runs the one table-driven walk ([`spec`], with the Swift call-site
//! reader in [`swift`]): a `File` node (index 0), definitions with 1-based spans and retained
//! qualified names (without the `{repository}:{path}` prefix), properties, `DEFINES`/
//! `DEFINES_METHOD` containment, `IMPORTS`, `CALLS` from the enclosing callable with receivers
//! and confidences, argument labels, ABI, inheritance and the Swift tree-sitter fallback
//! marking. C#, JavaScript and TypeScript (retained by other CodeGraph extractors, not by
//! `LanguageSpecs`) are `native` specs: the same walk without the baseline-extension markers.
//! A relationship whose target is a unique definition of the same file is `Inferred` locally;
//! any other is a symbol for the indexer to link or leave `Unresolved`.
//!
//! Parsing polls the run's stop signal and a per-file budget through Tree-sitter's progress
//! callback, so a cancelled run stops within one callback interval. Walks are iterative
//! (no recursion on deep trees) and bounded in nodes and edges.
use devworks_core::ports::indexing::{
    ExtractFault, ExtractedNode, FileExtract, LanguageInfo, LanguageMode, StructuralParser,
};
use serde_json::json;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};
use tree_sitter::{Language, Node, Parser};

mod spec;
mod swift;

/// Most nodes and edges kept per file (beyond them the file is recorded partial).
pub(super) const MAX_NODES: usize = 10_000;
pub(super) const MAX_EDGES: usize = 40_000;
/// Deepest syntax nesting walked.
const MAX_DEPTH: usize = 2_000;

/// What the C#, JavaScript and TypeScript specs read.
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
        let mut x = file_extract(path, root, language);
        match grammar_id {
            "sql" => super::domain::run(&mut x, stop, |o| super::domain::sql::extract(source, o))?,
            "terraform" => super::domain::run(&mut x, stop, |o| {
                super::domain::hcl::extract(path, source, o)
            })?,
            "yaml" => super::domain::run(&mut x, stop, |o| {
                super::domain::ansible::extract(path, source, root, repository, o)
            })?,
            "coldfusion" | "cfscript" => super::domain::run(&mut x, stop, |o| {
                super::domain::cfml::extract(path, source, o)
            })?,
            _ => match spec::spec(grammar_id) {
                Some(s) => spec::extract(s, source, path, root, &mut x, stop, deadline)?,
                None => {
                    return Err(ExtractFault::Failed(format!(
                        "no extractor for {grammar_id}"
                    )));
                }
            },
        }
        if stop() {
            return Err(ExtractFault::Cancelled);
        }
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
// The file node

/// The extraction of `path` so far: the file node (index 0) and the parse error count.
fn file_extract(path: &str, root: Node<'_>, language: &str) -> FileExtract {
    let mut out = FileExtract::default();
    out.nodes.push(ExtractedNode {
        kind: "File".into(),
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        qualified_name: path.to_owned(),
        span: spec::span(root),
        parent: None,
        properties: super::domain::props(&[("language", json!(language))]),
    });
    out.parse_errors = count_errors(root);
    out
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

// ---------------------------------------------------------------------------------------------
// Domain extractors: SQL, Terraform, Ansible and ColdFusion are ports of the retained CodeGraph
// extractors in `super::domain` (dispatched from `extract`).
