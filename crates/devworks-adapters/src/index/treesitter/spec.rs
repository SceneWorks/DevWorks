//! The retained generic Tree-sitter extraction (`TreeSitterExtractor` with `LanguageSpec`/
//! `LanguageSpecs`, frozen revision) for C, C++, Python, Go, Java, Ruby, Rust, PHP, Bash and Swift.
//!
//! Qualified names are the retained ones without the `{repository}:{path}` file prefix (the file
//! is the unit): `#type:Outer#type:Inner`, `#type:A#method:m(x:Int)`, `#function:f()`,
//! `#type:A#property:area`, and `#extension:Path#method:m()` for a member of a Swift extension of
//! a type declared in another file. Labels, property names and values, call confidences (0.7
//! bare, 0.65 resolved receiver, 0.45 unresolved receiver), receiver kinds and argument labels
//! are the retained ones. Inheritance edges come only from Swift's inheritance clause, as
//! retained; the other languages record a `superclass` property where their spec reads one.
//!
//! Retained pending edges become edges between nodes of the file (`DEFINES`/`DEFINES_METHOD`
//! `Resolved`; a relation to a local type `Inferred`); a relation to anything else is a `Symbol`
//! (`Unresolved`), and a retained source the file does not define (a Swift type reference, the
//! owner of a cross-file extension) is the file node with that source in `source_qualified_name`.
//! Retained unresolved calls are `CALLS` edges from the calling member: a unique same-file
//! callable they can bind to (by name, a local receiver type's members, and argument labels) is
//! `Inferred`, otherwise the callee name is a `Symbol`. Retained unresolved imports are `IMPORTS`
//! symbols from the file. Nodes emitted twice (the retained pipeline upserted by qualified name)
//! are merged per label and qualified name.
use super::swift;
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedEdge, ExtractedNode, FileExtract, Resolution, Span,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;
pub(super) use tree_sitter::Node;

/// The retained `TreeSitterExtractor.WalkerVersion`.
const WALKER_VERSION: u32 = 2;
/// The retained `GraphNodeConfidence.SwiftSemanticUnavailableReason`.
const SWIFT_SEMANTIC_UNAVAILABLE: &str = "swift_semantic_unavailable";

/// One retained `LanguageSpec` (its data; its hooks are matched on `id`).
pub(super) struct Spec {
    pub id: &'static str,
    /// The retained `ExtractionVersion`.
    pub version: u32,
    /// The retained `TreeSitterFallbackReason`: every node of the language is marked a
    /// tree-sitter fallback node.
    pub fallback_reason: Option<&'static str>,
    pub function_types: &'static [&'static str],
    pub class_types: &'static [&'static str],
    pub call_types: &'static [&'static str],
    pub call_target_fields: &'static [&'static str],
    pub call_receiver_fields: &'static [&'static str],
    pub import_types: &'static [&'static str],
    pub property_types: &'static [&'static str],
    pub name_field: &'static str,
    pub return_type_field: Option<&'static str>,
    pub parameters_field: Option<&'static str>,
    pub body_field: Option<&'static str>,
    pub superclass_field: Option<&'static str>,
    pub class_label: &'static str,
    pub function_label: &'static str,
}

const TARGETS: &[&str] = &["function", "name", "method"];
const RECEIVERS: &[&str] = &[
    "object", "receiver", "scope", "operand", "value", "path", "argument",
];

const BASE: Spec = Spec {
    id: "",
    version: 1,
    fallback_reason: None,
    function_types: &[],
    class_types: &[],
    call_types: &[],
    call_target_fields: TARGETS,
    call_receiver_fields: RECEIVERS,
    import_types: &[],
    property_types: &[],
    name_field: "name",
    return_type_field: None,
    parameters_field: None,
    body_field: None,
    superclass_field: None,
    class_label: "Class",
    function_label: "Function",
};

static SPECS: &[Spec] = &[
    Spec {
        id: "c",
        function_types: &["function_definition"],
        class_types: &["struct_specifier", "enum_specifier", "type_definition"],
        call_types: &["call_expression"],
        import_types: &["preproc_include"],
        return_type_field: Some("type"),
        body_field: Some("body"),
        class_label: "Struct",
        ..BASE
    },
    Spec {
        id: "cpp",
        // 2: functions with C language linkage (extern "C") record abi C.
        version: 2,
        function_types: &["function_definition"],
        class_types: &[
            "class_specifier",
            "struct_specifier",
            "enum_specifier",
            "type_definition",
            "namespace_definition",
        ],
        call_types: &["call_expression"],
        import_types: &["preproc_include"],
        return_type_field: Some("type"),
        body_field: Some("body"),
        ..BASE
    },
    Spec {
        id: "python",
        function_types: &["function_definition"],
        class_types: &["class_definition"],
        call_types: &["call"],
        import_types: &["import_statement", "import_from_statement"],
        return_type_field: Some("return_type"),
        parameters_field: Some("parameters"),
        body_field: Some("body"),
        superclass_field: Some("superclasses"),
        ..BASE
    },
    Spec {
        id: "go",
        // 3: Gin wildcard registrations expand to concrete HTTP methods.
        version: 3,
        function_types: &["function_declaration", "method_declaration"],
        class_types: &["type_declaration"],
        call_types: &["call_expression"],
        import_types: &["import_declaration"],
        return_type_field: Some("result"),
        parameters_field: Some("parameters"),
        body_field: Some("body"),
        ..BASE
    },
    Spec {
        id: "java",
        function_types: &["method_declaration", "constructor_declaration"],
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        call_types: &["method_invocation"],
        call_target_fields: &["name"],
        call_receiver_fields: &["object"],
        import_types: &["import_declaration"],
        return_type_field: Some("type"),
        parameters_field: Some("parameters"),
        body_field: Some("body"),
        superclass_field: Some("superclass"),
        function_label: "Method",
        ..BASE
    },
    Spec {
        id: "ruby",
        function_types: &["method", "singleton_method"],
        class_types: &["class", "module"],
        call_types: &["call", "method_call"],
        call_target_fields: &["method", "name"],
        call_receiver_fields: &["receiver", "object"],
        parameters_field: Some("parameters"),
        body_field: Some("body"),
        superclass_field: Some("superclass"),
        function_label: "Method",
        ..BASE
    },
    Spec {
        id: "rust",
        // 2: foreign ABI; 3: only when exported by symbol name; 5: Axum registrations.
        version: 5,
        function_types: &["function_item"],
        class_types: &["struct_item", "enum_item", "trait_item", "impl_item"],
        call_types: &["call_expression"],
        import_types: &["use_declaration"],
        return_type_field: Some("return_type"),
        parameters_field: Some("parameters"),
        body_field: Some("body"),
        ..BASE
    },
    Spec {
        id: "php",
        function_types: &["function_definition", "method_declaration"],
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
        ],
        call_types: &[
            "function_call_expression",
            "member_call_expression",
            "scoped_call_expression",
        ],
        call_target_fields: &["function", "name"],
        call_receiver_fields: &["object", "scope"],
        import_types: &["namespace_use_declaration"],
        return_type_field: Some("return_type"),
        parameters_field: Some("parameters"),
        body_field: Some("body"),
        ..BASE
    },
    Spec {
        id: "bash",
        function_types: &["function_definition"],
        call_types: &["command"],
        call_target_fields: &["name", "command"],
        body_field: Some("body"),
        ..BASE
    },
    Spec {
        id: "swift",
        // 2: @_exported imports are re-exports; 3: every node is a tree-sitter fallback node.
        version: 3,
        // No Swift semantic tier exists: tree-sitter output is fallback output.
        fallback_reason: Some(SWIFT_SEMANTIC_UNAVAILABLE),
        function_types: &[
            "function_declaration",
            "protocol_function_declaration",
            "init_declaration",
            "deinit_declaration",
            "subscript_declaration",
        ],
        class_types: &["class_declaration", "protocol_declaration"],
        property_types: &["property_declaration", "protocol_property_declaration"],
        call_types: &["call_expression", "constructor_expression"],
        import_types: &["import_declaration"],
        return_type_field: Some("return_type"),
        body_field: Some("body"),
        ..BASE
    },
];

/// The retained spec of a language ID (the languages of `LanguageSpecs`).
pub(super) fn spec(id: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.id == id)
}

// ---------------------------------------------------------------------------------------------
// Syntax helpers

pub(super) fn text(src: &[u8], n: Node<'_>) -> String {
    String::from_utf8_lossy(&src[n.start_byte().min(src.len())..n.end_byte().min(src.len())])
        .into_owned()
}
pub(super) fn child_by_field<'t>(n: Node<'t>, field: &str) -> Option<Node<'t>> {
    n.child_by_field_name(field)
}
pub(super) fn children(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.children(&mut c).collect()
}
pub(super) fn named_children(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}
/// The retained `GetFieldText`: a field's text, None when absent or empty.
fn field_text(src: &[u8], n: Node<'_>, field: &str) -> Option<String> {
    child_by_field(n, field)
        .map(|c| text(src, c))
        .filter(|t| !t.is_empty())
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

fn ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}
fn ident_part(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'!' | b'?')
}
/// The matches of the retained `\$?[A-Za-z_][A-Za-z0-9_!?]*`.
fn identifiers(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let mut j = i;
        if b[j] == b'$' {
            j += 1;
        }
        if j < b.len() && ident_start(b[j]) {
            j += 1;
            while j < b.len() && ident_part(b[j]) {
                j += 1;
            }
            out.push(&s[start..j]);
            i = j;
        } else {
            i += 1;
        }
    }
    out
}
/// The retained `^[A-Za-z_][A-Za-z0-9_!?]*(?:\.[A-Za-z_][A-Za-z0-9_!?]*)*$`.
fn is_receiver_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|seg| {
            let b = seg.as_bytes();
            !b.is_empty() && ident_start(b[0]) && b[1..].iter().all(|c| ident_part(*c))
        })
}
/// The retained `LastIdentifier`: the last identifier after dropping a trailing `<...>`.
fn last_identifier(t: &str) -> Option<String> {
    let mut t = t.trim();
    if t.ends_with('>')
        && let Some(open) = t.rfind('<')
        && !t[open + 1..t.len() - 1].contains('>')
    {
        t = &t[..open];
    }
    identifiers(t)
        .last()
        .map(|m| m.trim_start_matches('$').to_owned())
}

/// The retained `NormalizeSignature`: whitespace next to punctuation is dropped, whitespace
/// between two words collapses to one space; an absent or blank list is `()`.
pub(super) fn normalize_signature(parameters: Option<&str>) -> String {
    let Some(text) = parameters.map(str::trim).filter(|t| !t.is_empty()) else {
        return "()".into();
    };
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '\'' | '`' | '@');
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_whitespace() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut next = i;
        while next < chars.len() && chars[next].is_whitespace() {
            next += 1;
        }
        if next < chars.len() && out.chars().last().is_some_and(word) && word(chars[next]) {
            out.push(' ');
        }
        i = next;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Language hooks (the retained LanguageSpecs delegates)

/// `NameExtractor` (None falls back to `NameField`, as retained).
fn name_extractor(spec: &Spec, src: &[u8], n: Node<'_>) -> Option<String> {
    match spec.id {
        "c" => match n.kind() {
            "function_definition" => child_by_field(n, "declarator")
                .and_then(|d| child_by_field(d, "declarator"))
                .map(|i| text(src, i)),
            "type_definition" => child_by_field(n, "declarator").map(|d| text(src, d)),
            _ => child_by_field(n, "name").map(|x| text(src, x)),
        },
        "cpp" => match n.kind() {
            "function_definition" => {
                let ident = child_by_field(n, "declarator")
                    .and_then(|d| child_by_field(d, "declarator"))?;
                Some(
                    child_by_field(ident, "name")
                        .map(|x| text(src, x))
                        .unwrap_or_else(|| text(src, ident)),
                )
            }
            "type_definition" => child_by_field(n, "declarator").map(|d| text(src, d)),
            _ => child_by_field(n, "name").map(|x| text(src, x)),
        },
        "swift" => swift::callable_name(src, n),
        _ => None,
    }
}
fn get_name(spec: &Spec, src: &[u8], n: Node<'_>) -> Option<String> {
    name_extractor(spec, src, n).or_else(|| field_text(src, n, spec.name_field))
}
/// `ClassNameExtractor ?? GetName`.
fn class_name(spec: &Spec, src: &[u8], n: Node<'_>) -> Option<String> {
    let custom = match spec.id {
        "go" => named_children(n)
            .into_iter()
            .find_map(|c| child_by_field(c, "name"))
            .map(|x| text(src, x)),
        "rust" if n.kind() == "impl_item" => rust_nominal(src, child_by_field(n, "type")),
        "rust" => child_by_field(n, "name").map(|x| text(src, x)),
        "swift" => swift::type_name(src, n),
        _ => None,
    };
    custom.or_else(|| get_name(spec, src, n))
}
fn rust_nominal(src: &[u8], n: Option<Node<'_>>) -> Option<String> {
    let n = n?;
    match n.kind() {
        "generic_type" => rust_nominal(
            src,
            child_by_field(n, "type").or_else(|| {
                children(n)
                    .into_iter()
                    .find(|c| c.kind() != "type_arguments")
            }),
        ),
        "scoped_type_identifier" => rust_nominal(src, child_by_field(n, "name"))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| rust_nominal(src, child_by_field(n, "path"))),
        "type_identifier" | "identifier" => Some(text(src, n)),
        _ => children(n)
            .into_iter()
            .find_map(|c| rust_nominal(src, Some(c)).filter(|s| !s.trim().is_empty())),
    }
}
fn class_label(spec: &Spec, src: &[u8], n: Node<'_>) -> &'static str {
    match spec.id {
        "swift" => swift::class_label(src, n),
        _ => spec.class_label,
    }
}
fn is_container_only(spec: &Spec, src: &[u8], n: Node<'_>) -> bool {
    match spec.id {
        "rust" => n.kind() == "impl_item",
        "swift" => swift::declaration_kind(src, n).as_deref() == Some("extension"),
        _ => false,
    }
}
/// `ContainerOwnerReference`: (dotted path, reference qualified name).
fn container_owner(spec: &Spec, src: &[u8], n: Node<'_>) -> Option<(String, String)> {
    if spec.id != "swift" || swift::declaration_kind(src, n).as_deref() != Some("extension") {
        return None;
    }
    let path = swift::type_path(src, child_by_field(n, "name"))?;
    let reference = swift::type_ref(&path);
    Some((path, reference))
}
fn parameters(spec: &Spec, src: &[u8], n: Node<'_>) -> Option<String> {
    if spec.id == "swift" {
        return Some(swift::declaration_parameters(src, n));
    }
    spec.parameters_field.and_then(|f| field_text(src, n, f))
}
fn abi(spec: &Spec, src: &[u8], n: Node<'_>) -> Option<String> {
    match spec.id {
        "rust" => rust_abi(src, n),
        "cpp" => cpp_linkage(src, n),
        _ => None,
    }
}
/// The ABI of an `extern "abi" fn` (bare `extern fn` is C) exported under its own symbol name.
fn rust_abi(src: &[u8], n: Node<'_>) -> Option<String> {
    let modifier = named_children(n)
        .into_iter()
        .filter(|c| c.kind() == "function_modifiers")
        .flat_map(named_children)
        .find(|c| c.kind() == "extern_modifier")?;
    if !exported_by_symbol_name(src, n) {
        return None;
    }
    let abi = named_children(modifier)
        .into_iter()
        .find(|c| c.kind() == "string_literal")
        .map(|s| text(src, s).trim_matches('"').to_owned());
    match abi.as_deref() {
        None | Some("") => Some("C".into()),
        Some("Rust") => None,
        Some(a) => Some(a.to_owned()),
    }
}
fn exported_by_symbol_name(src: &[u8], item: Node<'_>) -> bool {
    let Some(parent) = item.parent() else {
        return false;
    };
    let siblings = children(parent);
    let Some(index) = siblings.iter().position(|s| {
        s.start_byte() == item.start_byte()
            && s.end_byte() == item.end_byte()
            && s.kind() == item.kind()
    }) else {
        return false;
    };
    for sibling in siblings[..index].iter().rev() {
        if matches!(sibling.kind(), "line_comment" | "block_comment") {
            continue;
        }
        if sibling.kind() != "attribute_item" {
            break;
        }
        let t = text(src, *sibling);
        if identifiers(&t)
            .iter()
            .any(|w| *w == "no_mangle" || *w == "export_name")
        {
            return true;
        }
    }
    false
}
/// `C` for a C++ function with C language linkage.
fn cpp_linkage(src: &[u8], n: Node<'_>) -> Option<String> {
    let mut parent = n.parent()?;
    if parent.kind() == "declaration_list" {
        parent = parent.parent()?;
    }
    if parent.kind() != "linkage_specification" {
        return None;
    }
    let linkage = child_by_field(parent, "value")
        .or_else(|| {
            named_children(parent)
                .into_iter()
                .find(|c| c.kind() == "string_literal")
        })
        .map(|v| text(src, v).trim_matches('"').to_owned());
    (linkage.as_deref() == Some("C")).then(|| "C".to_owned())
}

/// The retained `ExtractImportPath`.
fn import_path(src: &[u8], n: Node<'_>) -> Option<String> {
    for field in ["module_name", "source", "path", "name"] {
        if let Some(c) = child_by_field(n, field) {
            return Some(
                text(src, c)
                    .trim_matches(['"', '\'', '`', '<', '>'])
                    .to_owned(),
            );
        }
    }
    children(n)
        .into_iter()
        .find(|c| {
            matches!(
                c.kind(),
                "string"
                    | "interpreted_string_literal"
                    | "string_literal"
                    | "dotted_name"
                    | "scoped_identifier"
            )
        })
        .map(|c| text(src, c).trim_matches(['"', '\'', '`']).to_owned())
}

// ---------------------------------------------------------------------------------------------
// The walk

/// The cross-file owner a container-only definition's direct members attach to.
struct OwnerLink {
    source: String,
    origin_file: String,
}

#[derive(Clone)]
struct Ctx {
    parent: Rc<str>,
    class: Option<Rc<str>>,
    owner: Option<Rc<str>>,
    callable: Option<Rc<str>>,
    aliases: Rc<HashMap<String, String>>,
    pending: Option<Rc<OwnerLink>>,
    /// Syntax the retained walk never visits (a definition's non-body children: parameters,
    /// default values, inheritance clauses), visited only for baseline-extension calls.
    extra: bool,
    depth: usize,
}

struct PendingEdge {
    kind: &'static str,
    source: String,
    target: String,
    contain: bool,
    properties: Map<String, Value>,
}

struct CallRec {
    caller: String,
    callee: String,
    receiver: Option<String>,
    confidence: f32,
    kind: &'static str,
    labels: Option<String>,
    /// Recorded outside a callable (or in syntax the retained walk skips), as the previous
    /// structural walk did: a baseline extension.
    extension: bool,
}

struct W<'s, 't> {
    spec: &'static Spec,
    src: &'s [u8],
    rel: &'s str,
    out: &'s mut FileExtract,
    /// First node of each qualified name (the file is `""`).
    by_qn: HashMap<String, usize>,
    by_key: HashMap<(&'static str, String), usize>,
    /// Types declared in the file (qualified name to label), and simple/dotted names to the
    /// qualified name of the single type of that name.
    labels: HashMap<String, &'static str>,
    known: HashMap<String, String>,
    swift: Option<swift::Calls<'t>>,
    edges: Vec<PendingEdge>,
    calls: Vec<CallRec>,
    full: bool,
}

const MAX_NODES: usize = super::MAX_NODES;
const MAX_EDGES: usize = super::MAX_EDGES;
const MAX_DEPTH: usize = super::MAX_DEPTH;

/// Runs the retained generic extraction of `spec` over `root` into `out` (node 0 is the file).
pub(super) fn extract(
    spec: &'static Spec,
    src: &[u8],
    path: &str,
    root: Node<'_>,
    out: &mut FileExtract,
    stop: &(dyn Fn() -> bool + Sync),
    deadline: Instant,
) -> Result<(), ExtractFault> {
    out.nodes[0].properties.insert(
        "extraction_version".into(),
        json!(format!("{WALKER_VERSION}.{}", spec.version)),
    );
    let mut w = W {
        spec,
        src,
        rel: path,
        out,
        by_qn: HashMap::from([(String::new(), 0)]),
        by_key: HashMap::new(),
        labels: HashMap::new(),
        known: HashMap::new(),
        swift: None,
        edges: Vec::new(),
        calls: Vec::new(),
        full: false,
    };
    let mut ticks = 0usize;
    let mut tick = || -> Result<(), ExtractFault> {
        ticks += 1;
        if ticks.is_multiple_of(4096) {
            if stop() {
                return Err(ExtractFault::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(ExtractFault::TimedOut);
            }
        }
        Ok(())
    };
    w.collect_known_types(root, &mut tick)?;
    if spec.id == "swift" {
        w.swift = Some(swift::Calls::new(src, root));
    }
    if !spec.function_types.is_empty()
        || !spec.class_types.is_empty()
        || !spec.call_types.is_empty()
    {
        w.walk(root, &mut tick)?;
    }
    // The retained import walk (for languages with import nodes) and the baseline extensions.
    w.imports(root, &mut tick)?;
    w.finish();
    Ok(())
}

impl<'s, 't> W<'s, 't> {
    fn text(&self, n: Node<'_>) -> String {
        text(self.src, n)
    }

    // -- Known receiver types (the retained CollectKnownReceiverTypes) ----------------------

    fn collect_known_types(
        &mut self,
        root: Node<'t>,
        tick: &mut dyn FnMut() -> Result<(), ExtractFault>,
    ) -> Result<(), ExtractFault> {
        let mut ambiguous: HashSet<String> = HashSet::new();
        let mut extensions: Vec<Node<'t>> = Vec::new();
        let mut stack: Vec<(Node<'t>, String, bool, usize)> = vec![(root, String::new(), true, 0)];
        while let Some((n, parent, top, depth)) = stack.pop() {
            tick()?;
            if depth >= MAX_DEPTH {
                self.full = true;
                continue;
            }
            let class = self.spec.class_types.contains(&n.kind());
            let container_only = class && is_container_only(self.spec, self.src, n);
            if top && container_only && container_owner(self.spec, self.src, n).is_some() {
                extensions.push(n);
                continue;
            }
            let mut next_parent = parent.clone();
            let mut walk_target = n;
            if class && !container_only {
                let name = class_name(self.spec, self.src, n).filter(|s| !s.trim().is_empty());
                if let Some(name) = name {
                    let normalized = name.replace("::", ".");
                    let normalized = normalized.trim_matches('.');
                    if is_receiver_path(normalized) {
                        let qn = format!("{parent}#type:{name}");
                        self.labels
                            .insert(qn.clone(), class_label(self.spec, self.src, n));
                        let simple = normalized.rsplit('.').next().unwrap_or(normalized);
                        add_known(&mut self.known, &mut ambiguous, normalized, &qn);
                        add_known(&mut self.known, &mut ambiguous, simple, &qn);
                        if let Some(body) = self.spec.body_field.and_then(|f| child_by_field(n, f))
                        {
                            walk_target = body;
                        }
                        next_parent = qn;
                    }
                }
            }
            for c in named_children(walk_target).into_iter().rev() {
                stack.push((c, next_parent.clone(), top, depth + 1));
            }
        }
        // Members of an extension are named under the extended type when it is declared in this
        // file, else under `#extension:{path}`, as the definition walk names them.
        for extension in extensions {
            let Some((path, _)) = container_owner(self.spec, self.src, extension) else {
                continue;
            };
            let owner = self
                .find("", &path)
                .unwrap_or_else(|| format!("#extension:{path}"));
            let target = self
                .spec
                .body_field
                .and_then(|f| child_by_field(extension, f))
                .unwrap_or(extension);
            for c in named_children(target).into_iter().rev() {
                stack.push((c, owner.clone(), false, 1));
            }
            while let Some((n, parent, _, depth)) = stack.pop() {
                tick()?;
                if depth >= MAX_DEPTH {
                    self.full = true;
                    continue;
                }
                let mut next_parent = parent.clone();
                let mut walk_target = n;
                if self.spec.class_types.contains(&n.kind())
                    && !is_container_only(self.spec, self.src, n)
                    && let Some(name) =
                        class_name(self.spec, self.src, n).filter(|s| !s.trim().is_empty())
                {
                    let normalized = name.replace("::", ".");
                    let normalized = normalized.trim_matches('.');
                    if is_receiver_path(normalized) {
                        let qn = format!("{parent}#type:{name}");
                        self.labels
                            .insert(qn.clone(), class_label(self.spec, self.src, n));
                        let simple = normalized.rsplit('.').next().unwrap_or(normalized);
                        add_known(&mut self.known, &mut ambiguous, normalized, &qn);
                        add_known(&mut self.known, &mut ambiguous, simple, &qn);
                        if let Some(body) = self.spec.body_field.and_then(|f| child_by_field(n, f))
                        {
                            walk_target = body;
                        }
                        next_parent = qn;
                    }
                }
                for c in named_children(walk_target).into_iter().rev() {
                    stack.push((c, next_parent.clone(), false, depth + 1));
                }
            }
        }
        for a in ambiguous {
            self.known.remove(&a);
        }
        Ok(())
    }

    /// The retained `ResolveKnownReceiverType`.
    fn resolve_known(&self, receiver: &str) -> Option<String> {
        if let Some(q) = self.known.get(receiver) {
            return Some(q.clone());
        }
        if receiver.contains('.') {
            return None;
        }
        self.known.get(receiver).cloned()
    }

    /// The type at dotted `path` directly under `scope` (the retained `FileTypes.Find`).
    fn find(&self, scope: &str, path: &str) -> Option<String> {
        let qn = format!("{scope}#type:{}", path.replace('.', "#type:"));
        self.labels.contains_key(&qn).then_some(qn)
    }
    /// Lexical lookup from `scope` outward to the file's top level (`FileTypes.FindInScope`).
    fn find_in_scope(&self, scope: &str, path: &str) -> Option<String> {
        let mut scope = scope;
        loop {
            if let Some(f) = self.find(scope, path) {
                return Some(f);
            }
            let parent = scope.rfind('#')?;
            if scope.is_empty() {
                return None;
            }
            scope = &scope[..parent];
        }
    }

    // -- Output -----------------------------------------------------------------------------

    /// Adds (or merges into) node `kind`/`qn`, defined by `source`.
    fn node(
        &mut self,
        kind: &'static str,
        name: &str,
        qn: &str,
        n: Node<'_>,
        source: &str,
        properties: Map<String, Value>,
    ) {
        if let Some(&i) = self.by_key.get(&(kind, qn.to_owned())) {
            self.out.nodes[i].properties.extend(properties);
            return;
        }
        if self.out.nodes.len() >= MAX_NODES {
            self.full = true;
            return;
        }
        let parent = self.by_qn.get(source).copied().unwrap_or(0);
        self.out.nodes.push(ExtractedNode {
            kind: kind.into(),
            name: name.chars().take(512).collect(),
            qualified_name: qn.chars().take(2048).collect(),
            span: span(n),
            parent: Some(parent),
            properties,
        });
        let i = self.out.nodes.len() - 1;
        self.by_key.insert((kind, qn.to_owned()), i);
        self.by_qn.entry(qn.to_owned()).or_insert(i);
    }
    fn pending(
        &mut self,
        kind: &'static str,
        source: &str,
        target: &str,
        contain: bool,
        properties: Map<String, Value>,
    ) {
        if self.edges.len() >= MAX_EDGES {
            self.full = true;
            return;
        }
        self.edges.push(PendingEdge {
            kind,
            source: source.into(),
            target: target.into(),
            contain,
            properties,
        });
    }
    /// The retained `DefinitionEdge`: from `source`, or from a cross-file owner.
    fn definition(
        &mut self,
        source: &str,
        target: &str,
        kind: &'static str,
        owner: Option<&OwnerLink>,
    ) {
        match owner {
            None => self.pending(kind, source, target, true, Map::new()),
            Some(o) => {
                let mut p = Map::new();
                p.insert("origin_file".into(), json!(o.origin_file));
                let s = o.source.clone();
                self.pending(kind, &s, target, true, p);
            }
        }
    }

    fn walk(
        &mut self,
        root: Node<'t>,
        tick: &mut dyn FnMut() -> Result<(), ExtractFault>,
    ) -> Result<(), ExtractFault> {
        let top = Ctx {
            parent: Rc::from(""),
            class: None,
            owner: None,
            callable: None,
            aliases: Rc::new(HashMap::new()),
            pending: None,
            extra: false,
            depth: 0,
        };
        let mut stack: Vec<(Node<'t>, Ctx)> = vec![(root, top)];
        while let Some((n, ctx)) = stack.pop() {
            tick()?;
            let (target, child) = self.visit(n, &ctx);
            if child.depth >= MAX_DEPTH {
                self.full = true;
                continue;
            }
            if self.full && self.out.nodes.len() >= MAX_NODES {
                continue;
            }
            // A definition walks its body (retained); the rest of its syntax is visited for
            // baseline-extension calls only.
            if target.id() != n.id() {
                for c in named_children(n).into_iter().rev() {
                    if c.id() != target.id() {
                        stack.push((
                            c,
                            Ctx {
                                extra: true,
                                depth: child.depth + 1,
                                ..child.clone()
                            },
                        ));
                    }
                }
            }
            for c in named_children(target).into_iter().rev() {
                stack.push((
                    c,
                    Ctx {
                        depth: child.depth + 1,
                        ..child.clone()
                    },
                ));
            }
        }
        Ok(())
    }

    /// One step of the retained `WalkForDefinitionsAndCalls`: what `n` defines or calls, and the
    /// node whose children are walked next with which context.
    fn visit(&mut self, n: Node<'t>, ctx: &Ctx) -> (Node<'t>, Ctx) {
        let spec = self.spec;
        let kind = n.kind();
        if ctx.extra {
            if spec.call_types.contains(&kind) {
                let caller = ctx
                    .callable
                    .clone()
                    .or_else(|| ctx.class.clone())
                    .unwrap_or_else(|| ctx.parent.clone());
                self.call(n, &caller, ctx, true);
            }
            return (n, ctx.clone());
        }
        if spec.class_types.contains(&kind)
            && let Some(name) = class_name(spec, self.src, n)
        {
            return self.class_like(n, &name, ctx);
        }
        if ctx.owner.is_some()
            && ctx.callable.is_none()
            && spec.property_types.contains(&kind)
            && spec.id == "swift"
            && let Some((name, extra)) = swift::property(self.src, n)
        {
            let container: Rc<str> = ctx.class.clone().unwrap_or_else(|| ctx.parent.clone());
            let qn = format!("{container}#property:{name}");
            let mut p = Map::new();
            p.insert("confidence".into(), json!("high"));
            p.extend(extra);
            let source = ctx
                .pending
                .as_ref()
                .map(|o| o.source.clone())
                .unwrap_or_else(|| container.to_string());
            self.node("Property", &name, &qn, n, &source, p);
            self.definition(&container, &qn, "DEFINES", ctx.pending.as_deref());
            let qn: Rc<str> = Rc::from(qn);
            // Accessor bodies are owned by the property.
            return (
                n,
                Ctx {
                    parent: qn.clone(),
                    callable: Some(qn),
                    pending: None,
                    ..ctx.clone()
                },
            );
        }
        if spec.function_types.contains(&kind)
            && let Some(name) = get_name(spec, self.src, n)
        {
            return self.function_like(n, &name, ctx);
        }
        if spec.call_types.contains(&kind) {
            match ctx.callable.clone() {
                Some(callable) => self.call(n, &callable, ctx, false),
                // Outside any callable the retained walk records no call; the previous walk
                // owned it by the enclosing type or the file (a baseline extension).
                None => {
                    let caller = ctx.class.clone().unwrap_or_else(|| ctx.parent.clone());
                    self.call(n, &caller, ctx, true);
                }
            }
        }
        (n, ctx.clone())
    }

    fn class_like(&mut self, n: Node<'t>, name: &str, ctx: &Ctx) -> (Node<'t>, Ctx) {
        let spec = self.spec;
        let container: Rc<str> = ctx.class.clone().unwrap_or_else(|| ctx.parent.clone());
        let mut props = Map::new();
        props.insert("confidence".into(), json!("high"));
        let container_only = is_container_only(spec, self.src, n);
        // A container-only definition with an owner path (Swift extension) extends the top-level
        // type at that path; extensions only appear at file scope.
        let extended = if container_only {
            container_owner(spec, self.src, n)
        } else {
            None
        };
        let owner_qn: Option<String> = if !container_only {
            Some(format!("{container}#type:{name}"))
        } else if let Some((path, _)) = &extended {
            self.find("", path)
        } else {
            self.resolve_known(name)
        };
        if spec.id == "swift"
            && let Some(k) = swift::declaration_kind(self.src, n)
        {
            props.insert("declaration_kind".into(), json!(k));
        }
        if let Some(f) = spec.superclass_field
            && let Some(s) = field_text(self.src, n, f)
        {
            props.insert("superclass".into(), json!(s));
        }
        // A container-only definition whose owner is declared in another file: its members live
        // under `#extension:{path}` and their definition edges start at the owner reference.
        let cross_file = if owner_qn.is_none() { extended } else { None };
        let owner_link = cross_file.as_ref().map(|(_, reference)| {
            Rc::new(OwnerLink {
                source: reference.clone(),
                origin_file: self.rel.to_owned(),
            })
        });
        if spec.id == "swift"
            && let Some(type_qn) = owner_qn
                .clone()
                .or_else(|| cross_file.as_ref().map(|(_, r)| r.clone()))
        {
            // Names in an inheritance clause resolve outward from the declaring scope.
            let scope: String = if container_only {
                String::new()
            } else {
                container.to_string()
            };
            let resolve = |path: &str| {
                self.find_in_scope(&scope, path)
                    .map(|q| (q.clone(), self.labels[&q]))
            };
            let relations = swift::relations(
                self.src,
                n,
                &type_qn,
                self.rel,
                (!container_only).then_some(&mut props),
                &resolve,
            );
            for r in relations {
                self.pending(r.kind, &type_qn, &r.target, false, r.properties);
            }
        }
        if !container_only {
            let qn = owner_qn.clone().unwrap_or_default();
            let label = class_label(spec, self.src, n);
            let source = ctx
                .pending
                .as_ref()
                .map(|o| o.source.clone())
                .unwrap_or_else(|| ctx.parent.to_string());
            self.node(label, name, &qn, n, &source, props);
            self.definition(&ctx.parent.clone(), &qn, "DEFINES", ctx.pending.as_deref());
            for (edge, base) in self.baseline_bases(n) {
                self.pending(edge, &qn, &base, false, extension_props());
            }
        } else if spec.id == "rust"
            && let Some(owner) = &owner_qn
            && let Some(t) = rust_nominal(self.src, child_by_field(n, "trait"))
        {
            // `impl Trait for Type` with the type declared in the file (the previous walk).
            self.pending("IMPLEMENTS", owner, &t, false, extension_props());
        }
        let target = spec
            .body_field
            .and_then(|f| child_by_field(n, f))
            .unwrap_or(n);
        let member_parent: Rc<str> = match (&owner_qn, &cross_file) {
            (Some(q), _) => Rc::from(q.as_str()),
            (None, None) => ctx.parent.clone(),
            (None, Some((path, _))) => Rc::from(format!("{}#extension:{path}", ctx.parent)),
        };
        (
            target,
            Ctx {
                parent: member_parent,
                class: owner_qn.map(Rc::from),
                owner: Some(Rc::from(name)),
                callable: None,
                aliases: Rc::new(HashMap::new()),
                pending: owner_link,
                extra: false,
                depth: ctx.depth,
            },
        )
    }

    fn function_like(&mut self, n: Node<'t>, name: &str, ctx: &Ctx) -> (Node<'t>, Ctx) {
        let spec = self.spec;
        let (class_qn, owner, aliases) = self.callable_context(n, ctx);
        let container: Rc<str> = class_qn.clone().unwrap_or_else(|| ctx.parent.clone());
        let parameters = parameters(spec, self.src, n);
        let kind = if owner.is_none() {
            "function"
        } else {
            "method"
        };
        let qn = format!(
            "{container}#{kind}:{name}{}",
            normalize_signature(parameters.as_deref())
        );
        let label = if owner.is_some() {
            "Method"
        } else {
            spec.function_label
        };
        let mut props = Map::new();
        props.insert("confidence".into(), json!("high"));
        if let Some(f) = spec.return_type_field
            && let Some(r) = field_text(self.src, n, f)
        {
            props.insert("return_type".into(), json!(r));
        }
        if let Some(p) = &parameters {
            props.insert("parameters".into(), json!(p));
        }
        if let Some(o) = &owner {
            props.insert("receiver_owner".into(), json!(o.as_ref()));
        }
        if spec.id == "swift"
            && let Some(l) = swift::declaration_labels(self.src, n)
        {
            props.insert("argument_labels".into(), json!(l));
        }
        if let Some(a) = abi(spec, self.src, n) {
            props.insert("abi".into(), json!(a));
        }
        let source = ctx
            .pending
            .as_ref()
            .map(|o| o.source.clone())
            .unwrap_or_else(|| container.to_string());
        self.node(label, name, &qn, n, &source, props);
        self.definition(&container, &qn, "DEFINES_METHOD", ctx.pending.as_deref());
        // A nested definition takes ownership of calls in its own body.
        let target = spec
            .body_field
            .and_then(|f| child_by_field(n, f))
            .unwrap_or(n);
        let qn: Rc<str> = Rc::from(qn);
        (
            target,
            Ctx {
                parent: qn.clone(),
                class: class_qn,
                owner,
                callable: Some(qn),
                aliases,
                pending: None,
                extra: false,
                depth: ctx.depth,
            },
        )
    }

    /// The retained `GetCallableContext`: a Go method's receiver type (and its receiver
    /// variable), a C++ out-of-class definition's scope.
    #[allow(clippy::type_complexity)]
    fn callable_context(
        &self,
        n: Node<'t>,
        ctx: &Ctx,
    ) -> (
        Option<Rc<str>>,
        Option<Rc<str>>,
        Rc<HashMap<String, String>>,
    ) {
        let mut class = ctx.class.clone();
        let mut owner = ctx.owner.clone();
        let mut aliases = HashMap::new();
        if self.spec.id == "go" && n.kind() == "method_declaration" {
            let receiver = child_by_field(n, "receiver").map(|r| self.text(r));
            let ids: Vec<String> = receiver
                .as_deref()
                .map(|r| {
                    identifiers(r)
                        .into_iter()
                        .map(|m| m.trim_start_matches('$').to_owned())
                        .collect()
                })
                .unwrap_or_default();
            if !ids.is_empty() {
                let type_name = if ids.len() >= 2 { &ids[1] } else { &ids[0] };
                owner = Some(Rc::from(type_name.as_str()));
                let resolved = self.resolve_known(type_name);
                class = resolved.as_deref().map(Rc::from);
                if ids.len() >= 2 {
                    aliases.insert(
                        ids[0].clone(),
                        resolved.unwrap_or_else(|| type_name.clone()),
                    );
                }
            }
        }
        if class.is_none() && self.spec.id == "cpp" && n.kind() == "function_definition" {
            let scope = child_by_field(n, "declarator")
                .and_then(|d| child_by_field(d, "declarator"))
                .and_then(|d| child_by_field(d, "scope"))
                .map(|s| self.text(s));
            if let Some(scope) = scope.filter(|s| !s.trim().is_empty()) {
                let normalized = scope.replace("::", ".");
                let normalized = normalized.trim_matches('.');
                owner = Some(Rc::from(
                    normalized.rsplit('.').next().unwrap_or(normalized),
                ));
                class = self.resolve_known(normalized).as_deref().map(Rc::from);
            }
        }
        (class, owner, Rc::new(aliases))
    }

    fn call(&mut self, n: Node<'t>, callable: &str, ctx: &Ctx, extension: bool) {
        if self.spec.id == "swift" {
            let Some(reader) = &self.swift else {
                return;
            };
            let resolve = |path: &str| self.find_in_scope(callable, path);
            let cx = swift::CallContext {
                enclosing_type_qn: ctx.class.as_deref(),
                resolve_local_type: &resolve,
            };
            if let Some(site) = reader.read(n, &cx) {
                let confidence = match site.kind {
                    "Bare" => 0.7,
                    "Resolved" => 0.65,
                    _ => 0.45,
                };
                self.calls.push(CallRec {
                    caller: callable.to_owned(),
                    callee: site.callee,
                    receiver: site.receiver,
                    confidence,
                    kind: site.kind,
                    labels: Some(site.labels),
                    extension,
                });
            }
            return;
        }
        if let Some((callee, receiver, confidence, kind)) = self.call_target(n, ctx) {
            // Import-like commands are the baseline's imports, never its calls.
            if extension
                && ((self.spec.id == "ruby"
                    && kind == "Bare"
                    && matches!(callee.as_str(), "require" | "require_relative" | "load"))
                    || (self.spec.id == "bash" && callee == "source"))
            {
                return;
            }
            self.calls.push(CallRec {
                caller: callable.to_owned(),
                callee,
                receiver,
                confidence,
                kind,
                labels: None,
                extension,
            });
        }
    }

    /// The retained `ExtractCallTarget`.
    fn call_target(
        &self,
        n: Node<'t>,
        ctx: &Ctx,
    ) -> Option<(String, Option<String>, f32, &'static str)> {
        let spec = self.spec;
        let mut target = spec
            .call_target_fields
            .iter()
            .find_map(|f| child_by_field(n, f));
        let mut receiver = spec
            .call_receiver_fields
            .iter()
            .find_map(|f| child_by_field(n, f));
        // Bash command nodes expose the command name as a child rather than a field.
        if target.is_none() {
            target = children(n).into_iter().find(|c| {
                matches!(
                    c.kind(),
                    "command_name" | "word" | "identifier" | "constant"
                )
            });
        }
        let target = target.filter(|t| !spec.call_types.contains(&t.kind()))?;
        let name_node = child_by_field(target, "name")
            .or_else(|| child_by_field(target, "field"))
            .or_else(|| child_by_field(target, "method"));
        let callee = last_identifier(&self.text(name_node.unwrap_or(target)))?;
        if receiver.is_none() {
            receiver = RECEIVERS.iter().find_map(|f| child_by_field(target, f));
        }
        let scope = n.kind() == "scoped_call_expression"
            || matches!(target.kind(), "qualified_identifier" | "scoped_identifier");
        let (ty, resolved) = self.qualify_receiver(receiver.map(|r| self.text(r)), ctx, scope);
        let (confidence, kind) = match (receiver.is_some(), resolved) {
            (false, _) => (0.7, "Bare"),
            (true, true) => (0.65, "Resolved"),
            (true, false) => (0.45, "Unresolved"),
        };
        Some((callee, ty, confidence, kind))
    }

    /// The retained `QualifyReceiver`: (receiver type, resolved).
    fn qualify_receiver(
        &self,
        receiver: Option<String>,
        ctx: &Ctx,
        syntactic_scope: bool,
    ) -> (Option<String>, bool) {
        let Some(receiver) = receiver.filter(|r| !r.trim().is_empty()) else {
            return (None, false);
        };
        let trimmed = receiver.trim();
        if matches!(trimmed, "this" | "self" | "$this") {
            let owner = ctx.class.clone().or_else(|| ctx.owner.clone());
            let resolved = owner.is_some();
            return (owner.map(|o| o.to_string()), resolved);
        }
        if let Some(t) = ctx.aliases.get(trimmed.trim_start_matches('$')) {
            return (Some(t.clone()), true);
        }
        // Generic syntax cannot prove that an identifier used with `.` or `->` names a type:
        // only grammar-proven static scopes are eligible for owner resolution.
        if trimmed.starts_with('$') {
            return (None, false);
        }
        let normalized = trimmed.replace("::", ".").replace("->", ".");
        let normalized = normalized.trim_start_matches('$');
        if !is_receiver_path(normalized) || !syntactic_scope {
            return (None, false);
        }
        (
            Some(
                self.resolve_known(normalized)
                    .unwrap_or_else(|| normalized.to_owned()),
            ),
            true,
        )
    }

    /// The retained `WalkForImports`.
    fn imports(
        &mut self,
        root: Node<'t>,
        tick: &mut dyn FnMut() -> Result<(), ExtractFault>,
    ) -> Result<(), ExtractFault> {
        let mut emitted: HashSet<String> = HashSet::new();
        let mut stack: Vec<(Node<'t>, usize)> = vec![(root, 0)];
        while let Some((n, depth)) = stack.pop() {
            tick()?;
            let extras = self.baseline_imports(n);
            if self.spec.import_types.contains(&n.kind()) {
                let import = if self.spec.id == "swift" {
                    swift::read_import(self.src, n)
                } else {
                    import_path(self.src, n).map(|module| swift::Import {
                        module,
                        symbol: None,
                        symbol_kind: None,
                        exported: false,
                    })
                };
                if let Some(i) = import {
                    let mut p = Map::new();
                    if let Some(s) = i.symbol {
                        p.insert("symbol".into(), json!(s));
                    }
                    if let Some(k) = i.symbol_kind {
                        p.insert("symbol_kind".into(), json!(k));
                    }
                    if i.exported {
                        p.insert("exported".into(), json!(true));
                    }
                    emitted.insert(i.module.trim().to_owned());
                    self.import_edge(&i.module, p);
                }
            }
            let consumed = !extras.is_empty() || self.spec.import_types.contains(&n.kind());
            for m in extras {
                let m = m.trim().to_owned();
                if !m.is_empty() && emitted.insert(m.clone()) {
                    self.import_edge(&m, extension_props());
                }
            }
            if consumed {
                continue;
            }
            if depth >= MAX_DEPTH {
                self.full = true;
                continue;
            }
            for c in named_children(n).into_iter().rev() {
                stack.push((c, depth + 1));
            }
        }
        Ok(())
    }

    /// Modules the previous structural walk imported at `n` (the retained ones are skipped by the
    /// caller): every name of a Python `import`, Go import specs, PHP `use` clauses, a Rust `use`
    /// argument as written, a single-identifier Java import, a Swift import path as written, Ruby
    /// `require`/`require_relative`/`load` and Bash `source`/`.`.
    fn baseline_imports(&self, n: Node<'t>) -> Vec<String> {
        let src = self.src;
        let strip = |s: String| s.trim_matches(['"', '\'', '`', '<', '>']).to_owned();
        match (self.spec.id, n.kind()) {
            ("python", "import_statement") => named_children(n)
                .into_iter()
                .filter_map(|x| match x.kind() {
                    "dotted_name" => Some(text(src, x)),
                    "aliased_import" => child_by_field(x, "name").map(|m| text(src, m)),
                    _ => None,
                })
                .collect(),
            ("go", "import_declaration") => super::descendants_of(n, &["import_spec"])
                .into_iter()
                .filter_map(|s| child_by_field(s, "path").map(|p| strip(text(src, p))))
                .collect(),
            ("php", "namespace_use_declaration") => {
                // A grouped `use A\{B, C}` names `A\B` and `A\C`.
                let prefix = child_by_field(n, "body").and_then(|_| {
                    named_children(n)
                        .into_iter()
                        .find(|c| c.kind() == "namespace_name")
                        .map(|p| format!("{}\\", text(src, p)))
                });
                super::descendants_of(n, &["namespace_use_clause"])
                    .into_iter()
                    .map(|c| format!("{}{}", prefix.as_deref().unwrap_or(""), short(src, c)))
                    .collect()
            }
            ("rust", "use_declaration") => child_by_field(n, "argument")
                .map(|a| vec![short(src, a)])
                .unwrap_or_default(),
            ("java", "import_declaration") => named_children(n)
                .into_iter()
                .find(|x| matches!(x.kind(), "scoped_identifier" | "identifier"))
                .map(|x| vec![text(src, x)])
                .unwrap_or_default(),
            ("swift", "import_declaration") => named_children(n)
                .into_iter()
                .find(|x| x.kind() == "identifier")
                .map(|x| vec![text(src, x)])
                .unwrap_or_default(),
            ("ruby", "call")
                if child_by_field(n, "receiver").is_none()
                    && child_by_field(n, "method").is_some_and(|m| {
                        matches!(
                            text(src, m).as_str(),
                            "require" | "require_relative" | "load"
                        )
                    }) =>
            {
                child_by_field(n, "arguments")
                    .and_then(|a| {
                        super::descendants_of(a, &["string_content"])
                            .first()
                            .map(|s| text(src, *s))
                    })
                    .into_iter()
                    .collect()
            }
            ("bash", "command")
                if child_by_field(n, "name")
                    .is_some_and(|c| matches!(text(src, c).as_str(), "source" | ".")) =>
            {
                child_by_field(n, "argument")
                    .map(|a| vec![strip(text(src, a))])
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// Inheritance the previous structural walk read from a type's syntax (the retained code
    /// records only a `superclass` property for these languages): (edge kind, base name).
    fn baseline_bases(&self, n: Node<'t>) -> Vec<(&'static str, String)> {
        let src = self.src;
        let mut out = Vec::new();
        match self.spec.id {
            "python" => {
                if let Some(s) = child_by_field(n, "superclasses") {
                    for b in named_children(s) {
                        if matches!(b.kind(), "identifier" | "attribute") {
                            out.push(("INHERITS", text(src, b)));
                        }
                    }
                }
            }
            "java" => {
                if let Some(s) = child_by_field(n, "superclass") {
                    for t in
                        super::descendants_of(s, &["type_identifier", "scoped_type_identifier"])
                    {
                        out.push(("INHERITS", text(src, t)));
                    }
                }
                let edge = if n.kind() == "interface_declaration" {
                    "INHERITS"
                } else {
                    "IMPLEMENTS"
                };
                let interfaces = child_by_field(n, "interfaces").or_else(|| {
                    named_children(n)
                        .into_iter()
                        .find(|c| c.kind() == "extends_interfaces")
                });
                if let Some(s) = interfaces {
                    for t in super::descendants_of(s, &["type_identifier"]) {
                        out.push((edge, text(src, t)));
                    }
                }
            }
            "ruby" => {
                if let Some(s) = child_by_field(n, "superclass") {
                    out.push((
                        "INHERITS",
                        text(src, s).trim_start_matches('<').trim().to_owned(),
                    ));
                }
            }
            "cpp" => {
                if let Some(b) = named_children(n)
                    .into_iter()
                    .find(|c| c.kind() == "base_class_clause")
                {
                    for t in super::descendants_of(b, &["type_identifier", "qualified_identifier"])
                    {
                        out.push(("INHERITS", text(src, t)));
                    }
                }
            }
            "php" => {
                for (clause, edge) in [
                    ("base_clause", "INHERITS"),
                    ("class_interface_clause", "IMPLEMENTS"),
                ] {
                    if let Some(b) = named_children(n).into_iter().find(|c| c.kind() == clause) {
                        for t in super::descendants_of(b, &["name", "qualified_name"]) {
                            out.push((edge, text(src, t)));
                        }
                    }
                }
            }
            _ => {}
        }
        out.retain(|(_, b)| !b.trim().is_empty());
        out
    }

    fn import_edge(&mut self, module: &str, properties: Map<String, Value>) {
        let module = module.trim();
        if module.is_empty() {
            return;
        }
        self.push_edge(ExtractedEdge {
            kind: "IMPORTS".into(),
            source: 0,
            target: EdgeTarget::Symbol(module.to_owned()),
            resolution: Resolution::Unresolved,
            confidence: 1.0,
            properties,
        });
    }

    fn push_edge(&mut self, e: ExtractedEdge) {
        if self.out.edges.len() >= MAX_EDGES {
            self.full = true;
            return;
        }
        self.out.edges.push(e);
    }

    fn finish(mut self) {
        let mut seen: HashSet<(String, usize, String, String)> = HashSet::new();
        for e in std::mem::take(&mut self.edges) {
            let mut properties = e.properties;
            let source = match self.by_qn.get(&e.source) {
                Some(i) => *i,
                None => {
                    properties.insert("source_qualified_name".into(), json!(e.source));
                    0
                }
            };
            let (target, resolution, confidence) = match self.by_qn.get(&e.target).copied() {
                Some(i) if e.contain => (EdgeTarget::Local(i), Resolution::Resolved, 1.0),
                // A definition dropped by the node bound has no edge.
                None if e.contain => continue,
                Some(i) => (
                    EdgeTarget::Local(i),
                    Resolution::Inferred,
                    band(&properties),
                ),
                None => (
                    EdgeTarget::Symbol(e.target.clone()),
                    Resolution::Unresolved,
                    band(&properties),
                ),
            };
            let key = (
                e.kind.to_owned(),
                source,
                format!("{target:?}"),
                Value::Object(properties.clone()).to_string(),
            );
            if !seen.insert(key) {
                continue;
            }
            self.push_edge(ExtractedEdge {
                kind: e.kind.into(),
                source,
                target,
                resolution,
                confidence,
                properties,
            });
        }
        // Imports were recorded directly by the import walk; edge order is not significant.
        for c in std::mem::take(&mut self.calls) {
            self.call_edge(c);
        }
        if let Some(reason) = self.spec.fallback_reason {
            for n in self.out.nodes.iter_mut().skip(1) {
                n.properties.insert("confidence".into(), json!("low"));
                n.properties
                    .insert("extractionSource".into(), json!("tree-sitter-fallback"));
                n.properties.insert("fallbackReason".into(), json!(reason));
            }
        }
        if self.full {
            self.out.parse_errors += 1;
            self.out.nodes[0]
                .properties
                .insert("truncated".into(), json!(true));
        }
    }

    /// A retained unresolved call: `Inferred` to the unique same-file callable it can bind to,
    /// else a symbol named by the callee.
    fn call_edge(&mut self, c: CallRec) {
        let mut properties = if c.extension {
            extension_props()
        } else {
            Map::new()
        };
        properties.insert("receiver_kind".into(), json!(c.kind));
        if let Some(r) = &c.receiver {
            properties.insert("receiver_type".into(), json!(r));
        }
        if let Some(l) = &c.labels {
            properties.insert("argument_labels".into(), json!(l));
        }
        let source = match self.by_qn.get(&c.caller) {
            Some(i) => *i,
            None => {
                properties.insert("source_qualified_name".into(), json!(c.caller));
                0
            }
        };
        let nodes = &self.out.nodes;
        let owner = match c.kind {
            "Resolved" => c
                .receiver
                .as_deref()
                .and_then(|r| self.by_qn.get(r))
                .map(|t| Some(*t)),
            "Bare" => Some(None),
            _ => None,
        };
        let candidates: Vec<usize> = match owner {
            None => Vec::new(),
            Some(owner) => nodes
                .iter()
                .enumerate()
                .skip(1)
                .filter(|(_, n)| n.name == c.callee)
                .filter(|(_, n)| match owner {
                    // Members of the receiver type.
                    Some(t) => {
                        n.parent == Some(t) && matches!(n.kind.as_str(), "Function" | "Method")
                    }
                    // A callable, or a type the call constructs.
                    None => matches!(
                        n.kind.as_str(),
                        "Function" | "Method" | "Class" | "Struct" | "Record" | "Enum"
                    ),
                })
                .filter(
                    |(_, n)| match (&c.labels, n.properties.get("argument_labels")) {
                        (Some(call), Some(Value::String(declared))) => accepts(declared, call),
                        _ => true,
                    },
                )
                .map(|(i, _)| i)
                .collect(),
        };
        // The previous walk recorded a script-level Bash command only when it names a function
        // of the file (every other command is a program).
        if c.extension && self.spec.id == "bash" && candidates.is_empty() {
            return;
        }
        let (target, resolution) = if candidates.len() == 1 {
            (EdgeTarget::Local(candidates[0]), Resolution::Inferred)
        } else {
            if candidates.len() > 1 {
                properties.insert("candidates".into(), json!(candidates.len()));
            }
            (EdgeTarget::Symbol(c.callee.clone()), Resolution::Unresolved)
        };
        self.push_edge(ExtractedEdge {
            kind: "CALLS".into(),
            source,
            target,
            resolution,
            confidence: c.confidence,
            properties,
        });
    }
}

/// The marker of an edge the previous structural walk produced and the retained code does not.
fn extension_props() -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("baseline_extension".into(), json!(true));
    p
}

/// Whitespace collapsed to single spaces, at most 256 characters (the previous walk's form).
fn short(src: &[u8], n: Node<'_>) -> String {
    let t = text(src, n);
    t.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(256)
        .collect()
}

fn add_known(
    known: &mut HashMap<String, String>,
    ambiguous: &mut HashSet<String>,
    name: &str,
    qn: &str,
) {
    if ambiguous.contains(name) {
        return;
    }
    if known.get(name).is_some_and(|e| e != qn) {
        known.remove(name);
        ambiguous.insert(name.to_owned());
        return;
    }
    known.insert(name.to_owned(), qn.to_owned());
}

/// The retained confidence band of a relation (`high` 0.9, `medium` 0.6, `low` 0.3).
fn band(p: &Map<String, Value>) -> f32 {
    match p.get("confidence").and_then(Value::as_str) {
        Some("medium") => 0.6,
        Some("low") => 0.3,
        _ => 0.9,
    }
}

/// The retained `ArgumentLabels.Accepts`: whether a declaration's signature accepts a call's.
fn accepts(declared: &str, call: &str) -> bool {
    struct P {
        label: String,
        default: bool,
        variadic: bool,
    }
    let split = |s: &str| -> Vec<String> {
        s.split(':')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let params: Vec<P> = split(declared)
        .into_iter()
        .map(|mut seg| {
            let variadic = seg.ends_with("...");
            if variadic {
                seg.truncate(seg.len() - 3);
            }
            let default = seg.ends_with('=');
            if default {
                seg.pop();
            }
            P {
                label: seg,
                default,
                variadic,
            }
        })
        .collect();
    let args = split(call);
    fn matches(ps: &[P], p: usize, args: &[String], a: usize, budget: &mut usize) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if p == ps.len() {
            return a == args.len();
        }
        let cur = &ps[p];
        if (cur.default || cur.variadic) && matches(ps, p + 1, args, a, budget) {
            return true;
        }
        if a >= args.len() || !(args[a] == "*" || args[a] == cur.label) {
            return false;
        }
        if matches(ps, p + 1, args, a + 1, budget) {
            return true;
        }
        if !cur.variadic {
            return false;
        }
        let mut next = a + 1;
        while next < args.len() && args[next] == "_" {
            if matches(ps, p + 1, args, next + 1, budget) {
                return true;
            }
            next += 1;
        }
        false
    }
    let mut budget = 100_000;
    matches(&params, 0, &args, 0, &mut budget)
}
