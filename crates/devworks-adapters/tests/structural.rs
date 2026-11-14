//! Tree-sitter structural extraction golden inputs (CODE-01/02, CAP-LANGUAGES): every language
//! of the retained LanguageSpecs and the frozen adapter surface with a grammar, asserting exact
//! definitions, spans, containment, imports, calls and inheritance (positive and negative), the
//! honest text fallback, cancellation and the per-file budget. The LanguageSpecs languages are
//! golden against the retained generic walk (`TreeSitterExtractor`, `LanguageSpecs`,
//! `SwiftCallSites`): retained labels, qualified names without the `{repository}:{path}` prefix,
//! properties, call receivers, confidences and argument labels, Swift relations and the Swift
//! tree-sitter fallback marking.
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedEdge, FileExtract, LanguageMode, Resolution,
    StructuralParser,
};

fn extract(path: &str, src: &str) -> FileExtract {
    let p = TreeSitterParser::new();
    let lang = p.language_of(path).expect("a language").id;
    p.extract(lang, path, "repo", src.as_bytes(), &|| false, 5_000)
        .expect("extracts")
}
/// The node with qualified name `qn` (and kind), with its 1-based start and end lines.
fn def(x: &FileExtract, kind: &str, qn: &str) -> (usize, u32, u32) {
    let (i, n) = x
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| n.kind == kind && n.qualified_name == qn)
        .unwrap_or_else(|| {
            panic!(
                "no {kind} {qn}; have {:?}",
                x.nodes
                    .iter()
                    .map(|n| (&n.kind, &n.qualified_name))
                    .collect::<Vec<_>>()
            )
        });
    (i, n.span.start_line, n.span.end_line)
}
fn no_def(x: &FileExtract, qn: &str) {
    assert!(
        !x.nodes.iter().any(|n| n.qualified_name == qn),
        "unexpected {qn}"
    );
}
fn local(x: &FileExtract, kind: &str, from: usize, to: usize) -> bool {
    x.edges
        .iter()
        .any(|e| e.kind == kind && e.source == from && e.target == EdgeTarget::Local(to))
}
fn symbol(x: &FileExtract, kind: &str, from: usize, name: &str) -> Option<(Resolution, f32)> {
    x.edges
        .iter()
        .find(|e| e.kind == kind && e.source == from && e.target == EdgeTarget::Symbol(name.into()))
        .map(|e| (e.resolution, e.confidence))
}
/// Whether an edge is a baseline extension (an edge of the previous structural walk the
/// retained code does not produce).
fn is_ext(e: &ExtractedEdge) -> bool {
    e.properties.get("baseline_extension") == Some(&serde_json::json!(true))
}
/// The retained imports (sorted).
fn imports(x: &FileExtract) -> Vec<String> {
    import_names(x, false)
}
/// The baseline-extension imports (sorted).
fn ext_imports(x: &FileExtract) -> Vec<String> {
    import_names(x, true)
}
fn import_names(x: &FileExtract, ext: bool) -> Vec<String> {
    let mut v: Vec<String> = x
        .edges
        .iter()
        .filter(|e| e.kind == "IMPORTS" && is_ext(e) == ext)
        .filter_map(|e| match &e.target {
            EdgeTarget::Symbol(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

#[test]
fn coverage_is_explicit_with_pinned_grammars_and_honest_text_fallback() {
    let p = TreeSitterParser::new();
    for id in [
        "c",
        "cpp",
        "csharp",
        "python",
        "go",
        "java",
        "ruby",
        "rust",
        "php",
        "bash",
        "swift",
        "javascript",
        "typescript",
        "tsx",
        "sql",
        "terraform",
        "yaml",
        "coldfusion",
    ] {
        let l = p.languages().iter().find(|l| l.id == id).expect(id);
        assert_eq!(l.mode, LanguageMode::Structural, "{id}");
        assert!(
            l.grammar.is_some_and(|g| g.starts_with("tree-sitter-")),
            "{id}"
        );
        assert!(!l.capabilities.is_empty(), "{id}");
    }
    // Capabilities are the retained extent: inheritance edges only where the walk emits them.
    let caps = |id: &str| {
        p.languages()
            .iter()
            .find(|l| l.id == id)
            .unwrap()
            .capabilities
    };
    assert!(caps("swift").contains(&"inheritance") && caps("swift").contains(&"argument-labels"));
    for id in [
        "c", "cpp", "python", "go", "java", "ruby", "rust", "php", "bash",
    ] {
        assert!(!caps(id).contains(&"inheritance"), "{id}");
    }
    for id in ["go", "php", "ruby", "bash"] {
        assert!(!caps(id).contains(&"imports"), "{id}");
    }
    assert!(caps("rust").contains(&"abi") && caps("cpp").contains(&"abi"));
    let k = p.language_of("app/Main.kt").unwrap();
    assert_eq!(k.mode, LanguageMode::TextFallback);
    assert!(k.grammar.is_none() && k.note.is_some());
    assert!(
        p.language_of("README.md").is_none(),
        "prose is not a code language"
    );
    assert!(
        p.language_of(".bashrc").is_none(),
        "a dot file has no extension"
    );
    assert_eq!(
        p.language_of("SRC/LIB.RS").unwrap().id,
        "rust",
        "extensions compare case-insensitively"
    );
    assert!(matches!(
        p.extract("kotlin", "a.kt", "repo", b"fun x() {}", &|| false, 100),
        Err(ExtractFault::Failed(_))
    ));
    assert!(
        matches!(
            p.extract("rust", "a.rs", "repo", b"fn a() {}\0\0", &|| false, 100),
            Err(ExtractFault::Failed(_))
        ),
        "binary content is never parsed"
    );
}

#[test]
fn parsing_stops_for_cancellation_and_budget() {
    let p = TreeSitterParser::new();
    let big: String = (0..20_000)
        .map(|i| format!("fn f{i}() {{ g{i}(); }}\n"))
        .collect();
    assert_eq!(
        p.extract("rust", "big.rs", "repo", big.as_bytes(), &|| true, 60_000),
        Err(ExtractFault::Cancelled)
    );
    assert!(matches!(
        p.extract("rust", "big.rs", "repo", big.as_bytes(), &|| false, 1),
        Err(ExtractFault::TimedOut) | Ok(_)
    ));
}

/// A string property of node `i`.
fn prop<'a>(x: &'a FileExtract, i: usize, key: &str) -> Option<&'a str> {
    x.nodes[i].properties.get(key).and_then(|v| v.as_str())
}
/// The retained `CALLS` edges from `from` to `target`.
fn calls(x: &FileExtract, from: usize, target: EdgeTarget) -> Vec<&ExtractedEdge> {
    x.edges
        .iter()
        .filter(|e| e.kind == "CALLS" && e.source == from && e.target == target && !is_ext(e))
        .collect()
}
/// The baseline-extension edges of `kind` from `from`: (target, confidence).
fn ext(x: &FileExtract, kind: &str, from: usize) -> Vec<(EdgeTarget, f32)> {
    x.edges
        .iter()
        .filter(|e| e.kind == kind && e.source == from && is_ext(e))
        .map(|e| (e.target.clone(), e.confidence))
        .collect()
}
/// The single `CALLS` edge from `from` to `target`.
fn call(x: &FileExtract, from: usize, target: EdgeTarget) -> &ExtractedEdge {
    let all = calls(x, from, target.clone());
    assert_eq!(all.len(), 1, "one call {from} -> {target:?}; have {all:?}");
    all[0]
}
fn sym(name: &str) -> EdgeTarget {
    EdgeTarget::Symbol(name.into())
}
fn eprop<'a>(e: &'a ExtractedEdge, key: &str) -> Option<&'a str> {
    e.properties.get(key).and_then(|v| v.as_str())
}
/// The retained call: resolution, confidence, receiver kind and receiver type.
fn shape(e: &ExtractedEdge) -> (Resolution, f32, Option<&str>, Option<&str>) {
    (
        e.resolution,
        e.confidence,
        eprop(e, "receiver_kind"),
        eprop(e, "receiver_type"),
    )
}
fn no_relations(x: &FileExtract) {
    assert!(
        !x.edges
            .iter()
            .any(|e| matches!(e.kind.as_str(), "INHERITS" | "IMPLEMENTS") && !is_ext(e)),
        "the retained generic walk records inheritance only for Swift"
    );
}

#[test]
fn rust_definitions_impls_calls_imports_and_abi() {
    let x = extract(
        "src/lib.rs",
        "use crate::a::b;\nuse std::{x, y};\nmod m { pub fn inner() {} }\npub struct S;\npub trait T { fn d(&self) {} }\nimpl T for S {\n    fn f(&self) {\n        helper();\n        self.g();\n        other.h();\n        S::new();\n        crate::a::z();\n    }\n    fn g(&self) {}\n}\nfn helper() {}\n#[no_mangle]\npub extern \"C\" fn exported() {}\nextern \"C\" fn callback() {}\n#[export_name = \"x\"]\nextern \"system\" fn sys(a: *const u8) {}\n",
    );
    assert_eq!(prop(&x, 0, "extraction_version"), Some("2.5"));
    let (s, l, _) = def(&x, "Class", "#type:S");
    assert_eq!(l, 4, "a Rust struct is a Class, as retained");
    let (t, ..) = def(&x, "Class", "#type:T");
    let (d, ..) = def(&x, "Method", "#type:T#method:d(&self)");
    assert!(local(&x, "DEFINES_METHOD", t, d));
    let (f, fl, fe) = def(&x, "Method", "#type:S#method:f(&self)");
    assert_eq!((fl, fe), (7, 13));
    assert_eq!(prop(&x, f, "receiver_owner"), Some("S"));
    assert_eq!(prop(&x, f, "parameters"), Some("(&self)"));
    assert_eq!(prop(&x, f, "confidence"), Some("high"));
    let (g, ..) = def(&x, "Method", "#type:S#method:g(&self)");
    let (h, ..) = def(&x, "Function", "#function:helper()");
    def(&x, "Function", "#function:inner()");
    assert!(
        x.nodes.iter().all(|n| n.kind != "Module"),
        "a mod is no container in the retained spec"
    );
    assert!(local(&x, "DEFINES", 0, s) && local(&x, "DEFINES", 0, t));
    assert!(
        local(&x, "DEFINES_METHOD", s, f),
        "impl members attach to the local type; the impl emits no node"
    );
    no_relations(&x);
    assert_eq!(
        shape(call(&x, f, EdgeTarget::Local(h))),
        (Resolution::Inferred, 0.7, Some("Bare"), None)
    );
    assert_eq!(
        shape(call(&x, f, EdgeTarget::Local(g))),
        (
            Resolution::Inferred,
            0.65,
            Some("Resolved"),
            Some("#type:S")
        )
    );
    assert_eq!(
        shape(call(&x, f, sym("h"))),
        (Resolution::Unresolved, 0.45, Some("Unresolved"), None)
    );
    assert_eq!(
        shape(call(&x, f, sym("new"))),
        (
            Resolution::Unresolved,
            0.65,
            Some("Resolved"),
            Some("#type:S")
        ),
        "a scoped path names a local type"
    );
    assert_eq!(
        shape(call(&x, f, sym("z"))),
        (
            Resolution::Unresolved,
            0.65,
            Some("Resolved"),
            Some("crate.a")
        )
    );
    assert_eq!(
        imports(&x),
        vec!["crate::a::b".to_owned()],
        "the retained reader reads a path, not a use list"
    );
    // Baseline extensions (the previous structural walk): additive and marked.
    assert_eq!(ext(&x, "IMPLEMENTS", s), vec![(sym("T"), 0.9)]);
    assert_eq!(ext_imports(&x), vec!["std::{x, y}".to_owned()]);
    assert!(
        x.edges
            .iter()
            .filter(|e| is_ext(e))
            .all(|e| e.resolution == Resolution::Unresolved),
        "{:?}",
        x.edges
    );
    let (e, ..) = def(&x, "Function", "#function:exported()");
    assert_eq!(prop(&x, e, "abi"), Some("C"));
    let (cb, ..) = def(&x, "Function", "#function:callback()");
    assert!(
        x.nodes[cb].properties.get("abi").is_none(),
        "an unexported extern fn keeps no symbol ABI"
    );
    let (sy, ..) = def(&x, "Function", "#function:sys(a:*const u8)");
    assert_eq!(prop(&x, sy, "abi"), Some("system"));
    assert_eq!(x.parse_errors, 0);
}

#[test]
fn python_classes_superclasses_imports_and_calls() {
    let x = extract(
        "pkg/mod.py",
        "import os, a.b as c\nfrom x.y import z\n\nclass A(Base, Mixin):\n    def m(self, k: int) -> int:\n        self.n()\n        f()\n        A()\n    def n(self):\n        pass\n\ndef f():\n    os.path.join('a')\n",
    );
    let (a, l, e) = def(&x, "Class", "#type:A");
    assert_eq!((l, e), (4, 10));
    assert_eq!(prop(&x, a, "superclass"), Some("(Base, Mixin)"));
    no_relations(&x);
    let (m, ..) = def(&x, "Method", "#type:A#method:m(self,k:int)");
    assert_eq!(prop(&x, m, "parameters"), Some("(self, k: int)"));
    assert_eq!(prop(&x, m, "return_type"), Some("int"));
    let (n, ..) = def(&x, "Method", "#type:A#method:n(self)");
    let (f, ..) = def(&x, "Function", "#function:f()");
    assert_eq!(
        shape(call(&x, m, EdgeTarget::Local(n))),
        (
            Resolution::Inferred,
            0.65,
            Some("Resolved"),
            Some("#type:A")
        )
    );
    assert_eq!(call(&x, m, EdgeTarget::Local(f)).confidence, 0.7);
    assert_eq!(
        call(&x, m, EdgeTarget::Local(a)).resolution,
        Resolution::Inferred,
        "a bare call of a class constructs it"
    );
    assert_eq!(
        shape(call(&x, f, sym("join"))),
        (Resolution::Unresolved, 0.45, Some("Unresolved"), None)
    );
    assert_eq!(
        imports(&x),
        vec!["os".to_owned(), "x.y".into()],
        "the retained reader reads the first name of an import statement"
    );
    assert_eq!(ext_imports(&x), vec!["a.b".to_owned()]);
    assert_eq!(
        ext(&x, "INHERITS", a),
        vec![(sym("Base"), 0.9), (sym("Mixin"), 0.9)]
    );
}

#[test]
fn c_and_cpp_definitions_includes_namespaces_and_linkage() {
    let c = extract(
        "src/main.c",
        "#include <stdio.h>\n#include \"util.h\"\nstruct S { int a; };\nstruct S *decl;\ntypedef struct { int b; } T;\nenum E { X };\nstatic int helper(void) { return 1; }\nint main(void) { helper(); printf(\"x\"); return 0; }\n",
    );
    let (s, sl, _) = def(&c, "Struct", "#type:S");
    assert_eq!(sl, 3);
    assert_eq!(
        c.nodes.iter().filter(|n| n.name == "S").count(),
        1,
        "a second emission of a qualified name merges into the first"
    );
    assert!(local(&c, "DEFINES", 0, s));
    def(&c, "Struct", "#type:T");
    def(&c, "Struct", "#type:E");
    let (h, ..) = def(&c, "Function", "#function:helper()");
    assert_eq!(prop(&c, h, "return_type"), Some("int"));
    let (m, ml, _) = def(&c, "Function", "#function:main()");
    assert_eq!(ml, 8);
    assert_eq!(
        shape(call(&c, m, EdgeTarget::Local(h))),
        (Resolution::Inferred, 0.7, Some("Bare"), None)
    );
    assert_eq!(
        call(&c, m, sym("printf")).resolution,
        Resolution::Unresolved
    );
    assert_eq!(imports(&c), vec!["stdio.h".to_owned(), "util.h".into()]);
    assert_eq!(prop(&c, 0, "extraction_version"), Some("2.1"));
    assert!(
        c.edges.iter().all(|e| !is_ext(e)),
        "nothing beyond the retained C output"
    );

    let cpp = extract(
        "src/a.cpp",
        "#include \"a.h\"\nnamespace n {\nclass C : public B { void m(); };\n}\nvoid n::C::m() { f(); this->g(); }\nextern \"C\" int e() { return 0; }\nint f() { return 1; }\nextern \"C\" { int e2() { return 0; } }\nstruct D { void k(); };\nvoid D::k() { this->k(); }\n",
    );
    let (ns, ..) = def(&cpp, "Class", "#type:n");
    let (cls, ..) = def(&cpp, "Class", "#type:n#type:C");
    assert!(local(&cpp, "DEFINES", ns, cls));
    no_relations(&cpp);
    // The retained name of an out-of-class definition is its declarator's `name`; the owner is
    // its outermost scope.
    let (m, ..) = def(&cpp, "Method", "#type:n#method:C::m()");
    assert_eq!(prop(&cpp, m, "receiver_owner"), Some("n"));
    assert!(local(&cpp, "DEFINES_METHOD", ns, m));
    let (f, ..) = def(&cpp, "Function", "#function:f()");
    assert!(cpp.nodes[f].properties.get("abi").is_none());
    assert_eq!(call(&cpp, m, EdgeTarget::Local(f)).confidence, 0.7);
    assert_eq!(
        shape(call(&cpp, m, sym("g"))),
        (
            Resolution::Unresolved,
            0.65,
            Some("Resolved"),
            Some("#type:n")
        )
    );
    let (e, ..) = def(&cpp, "Function", "#function:e()");
    assert_eq!(prop(&cpp, e, "abi"), Some("C"));
    let (e2, ..) = def(&cpp, "Function", "#function:e2()");
    assert_eq!(
        prop(&cpp, e2, "abi"),
        Some("C"),
        "inside an extern \"C\" block"
    );
    let (dd, ..) = def(&cpp, "Class", "#type:D");
    let (k, ..) = def(&cpp, "Method", "#type:D#method:k()");
    assert!(local(&cpp, "DEFINES_METHOD", dd, k));
    assert_eq!(
        shape(call(&cpp, k, EdgeTarget::Local(k))),
        (
            Resolution::Inferred,
            0.65,
            Some("Resolved"),
            Some("#type:D")
        )
    );
    assert_eq!(imports(&cpp), vec!["a.h".to_owned()]);
    assert_eq!(ext(&cpp, "INHERITS", cls), vec![(sym("B"), 0.9)]);
    assert_eq!(prop(&cpp, 0, "extraction_version"), Some("2.2"));
}

#[test]
fn csharp_types_members_and_inheritance() {
    // C# is outside the retained LanguageSpecs (Roslyn in CodeGraph): a native spec of the same
    // engine, so qualified names are the engine's.
    let cs = extract(
        "src/A.cs",
        "using System.Text;\nnamespace N {\n  public class A : Base, IThing {\n    public A() {}\n    public void M() { Foo(); x.Bar(); }\n    public int P { get; set; }\n    void Foo() {}\n  }\n  interface IThing {}\n  record R(int X);\n}\n",
    );
    let (a, ..) = def(&cs, "Class", "#type:N#type:A");
    def(&cs, "Constructor", "#type:N#type:A#method:A()");
    let (m, ..) = def(&cs, "Method", "#type:N#type:A#method:M()");
    let (foo, ..) = def(&cs, "Method", "#type:N#type:A#method:Foo()");
    def(&cs, "Property", "#type:N#type:A#property:P");
    def(&cs, "Interface", "#type:N#type:IThing");
    def(&cs, "Record", "#type:N#type:R");
    assert!(local(&cs, "CALLS", m, foo));
    assert_eq!(
        symbol(&cs, "INHERITS", a, "Base").map(|s| s.0),
        Some(Resolution::Unresolved)
    );
    assert!(
        symbol(&cs, "IMPLEMENTS", a, "IThing").is_some(),
        "an I-prefixed base is an interface"
    );
    assert_eq!(imports(&cs), vec!["System.Text".to_owned()]);
}

#[test]
fn java_and_go_types_members_receivers_and_imports() {
    let java = extract(
        "src/A.java",
        "import a.b.C;\nimport static a.b.D.*;\nclass A extends B implements I, J { A(int v) {} void m() { f(); x.g(); this.f(); } void f() {} }\ninterface I extends K {}\nenum E { X }\n",
    );
    let (a, ..) = def(&java, "Class", "#type:A");
    assert_eq!(prop(&java, a, "superclass"), Some("extends B"));
    no_relations(&java);
    let (ctor, ..) = def(&java, "Method", "#type:A#method:A(int v)");
    assert!(local(&java, "DEFINES_METHOD", a, ctor));
    let (m, ..) = def(&java, "Method", "#type:A#method:m()");
    assert_eq!(prop(&java, m, "return_type"), Some("void"));
    let (f, ..) = def(&java, "Method", "#type:A#method:f()");
    let to_f = calls(&java, m, EdgeTarget::Local(f));
    let mut confidences: Vec<f32> = to_f.iter().map(|e| e.confidence).collect();
    confidences.sort_by(f32::total_cmp);
    assert_eq!(confidences, vec![0.65, 0.7], "bare `f()` and `this.f()`");
    assert_eq!(
        shape(call(&java, m, sym("g"))),
        (Resolution::Unresolved, 0.45, Some("Unresolved"), None)
    );
    assert_eq!(ext(&java, "INHERITS", a), vec![(sym("B"), 0.9)]);
    assert_eq!(
        ext(&java, "IMPLEMENTS", a),
        vec![(sym("I"), 0.9), (sym("J"), 0.9)]
    );
    let (i, ..) = def(&java, "Class", "#type:I");
    assert_eq!(
        ext(&java, "INHERITS", i),
        vec![(sym("K"), 0.9)],
        "an interface extends"
    );
    def(&java, "Class", "#type:E");
    assert_eq!(imports(&java), vec!["a.b.C".to_owned(), "a.b.D".into()]);

    let go = extract(
        "pkg/s.go",
        "package p\nimport (\n\t\"fmt\"\n\tx \"a/b\"\n)\ntype S struct{}\ntype I interface{}\nfunc (s *S) M() { fmt.Println(); f(); s.N() }\nfunc (s *S) N() {}\nfunc f() {}\n",
    );
    assert_eq!(prop(&go, 0, "extraction_version"), Some("2.3"));
    let (s, ..) = def(&go, "Class", "#type:S");
    def(&go, "Class", "#type:I");
    let (m, ..) = def(&go, "Method", "#type:S#method:M()");
    assert_eq!(prop(&go, m, "receiver_owner"), Some("S"));
    let (n, ..) = def(&go, "Method", "#type:S#method:N()");
    let (f, ..) = def(&go, "Function", "#function:f()");
    assert!(
        local(&go, "DEFINES_METHOD", s, m),
        "a Go method attaches to its receiver type"
    );
    assert_eq!(call(&go, m, EdgeTarget::Local(f)).confidence, 0.7);
    assert_eq!(
        shape(call(&go, m, EdgeTarget::Local(n))),
        (
            Resolution::Inferred,
            0.65,
            Some("Resolved"),
            Some("#type:S")
        ),
        "the receiver variable aliases its type"
    );
    assert_eq!(
        shape(call(&go, m, sym("Println"))),
        (Resolution::Unresolved, 0.45, Some("Unresolved"), None)
    );
    assert!(
        imports(&go).is_empty(),
        "the retained import reader reads nothing from a Go import declaration"
    );
    assert_eq!(ext_imports(&go), vec!["a/b".to_owned(), "fmt".into()]);
}

#[test]
fn ruby_php_and_bash() {
    let rb = extract(
        "lib/a.rb",
        "require 'json'\nclass A < B\n  def m\n    foo(1)\n    x.bar\n    self.foo(2)\n  end\n  def foo(x); end\nend\nmodule M; end\n",
    );
    let (a, ..) = def(&rb, "Class", "#type:A");
    assert_eq!(prop(&rb, a, "superclass"), Some("< B"));
    no_relations(&rb);
    let (m, ..) = def(&rb, "Method", "#type:A#method:m()");
    let (foo, ..) = def(&rb, "Method", "#type:A#method:foo(x)");
    def(&rb, "Class", "#type:M");
    let mut to_foo: Vec<(f32, Option<&str>)> = calls(&rb, m, EdgeTarget::Local(foo))
        .iter()
        .map(|e| (e.confidence, eprop(e, "receiver_kind")))
        .collect();
    to_foo.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(to_foo, vec![(0.65, Some("Resolved")), (0.7, Some("Bare"))]);
    assert_eq!(call(&rb, m, sym("bar")).confidence, 0.45);
    assert!(
        imports(&rb).is_empty() && !rb.edges.iter().any(|e| e.target == sym("require")),
        "`require` is a call outside any callable, not an import"
    );
    assert_eq!(ext_imports(&rb), vec!["json".to_owned()]);
    assert_eq!(ext(&rb, "INHERITS", a), vec![(sym("B"), 0.9)]);
    assert!(
        ext(&rb, "CALLS", 0).is_empty(),
        "a baseline `require` is an import, not a call"
    );

    let php = extract(
        "src/a.php",
        "<?php\nuse A\\B;\nclass C extends D implements E { function m() { f(); $this->g(); X::y(); self::g(); } function g() {} }\nfunction f() {}\n",
    );
    let (c, ..) = def(&php, "Class", "#type:C");
    assert!(php.nodes[c].properties.get("superclass").is_none());
    no_relations(&php);
    let (m, ..) = def(&php, "Method", "#type:C#method:m()");
    let (g, ..) = def(&php, "Method", "#type:C#method:g()");
    let (f, ..) = def(&php, "Function", "#function:f()");
    assert_eq!(call(&php, m, EdgeTarget::Local(f)).confidence, 0.7);
    let on_self = calls(&php, m, EdgeTarget::Local(g));
    assert_eq!(on_self.len(), 2, "`$this->g()` and `self::g()`");
    assert!(on_self.iter().all(|e| e.confidence == 0.65));
    assert_eq!(
        shape(call(&php, m, sym("y"))),
        (Resolution::Unresolved, 0.65, Some("Resolved"), Some("X")),
        "a static scope is a grammar-proven receiver"
    );
    assert!(imports(&php).is_empty());
    assert_eq!(ext_imports(&php), vec!["A\\B".to_owned()]);
    assert_eq!(ext(&php, "INHERITS", c), vec![(sym("D"), 0.9)]);
    assert_eq!(ext(&php, "IMPLEMENTS", c), vec![(sym("E"), 0.9)]);

    let sh = extract(
        "bin/run.sh",
        "source ./lib.sh\nf() { echo hi; }\nfunction g { f; }\ng\n",
    );
    let (f, ..) = def(&sh, "Function", "#function:f()");
    let (g, ..) = def(&sh, "Function", "#function:g()");
    assert_eq!(
        shape(call(&sh, g, EdgeTarget::Local(f))),
        (Resolution::Inferred, 0.7, Some("Bare"), None)
    );
    assert_eq!(
        shape(call(&sh, f, sym("echo"))),
        (Resolution::Unresolved, 0.45, Some("Unresolved"), None),
        "a command's argument is read as its receiver, as retained"
    );
    assert!(
        calls(&sh, 0, EdgeTarget::Local(g)).is_empty(),
        "retained calls are recorded only inside callables"
    );
    assert_eq!(
        ext(&sh, "CALLS", 0),
        vec![(EdgeTarget::Local(g), 0.7)],
        "the script calls a function of the file; `source` is no call"
    );
    assert!(imports(&sh).is_empty());
    assert_eq!(ext_imports(&sh), vec!["./lib.sh".to_owned()]);
}

#[test]
fn swift_types_members_properties_relations_and_fallback_marking() {
    let sw = extract(
        "Sources/A.swift",
        "@_exported import Foundation\nimport struct Geometry.Point\n@testable import App.Core\nprotocol Shape: AnyObject, Drawable {\n  func area() -> Double\n  var name: String { get }\n}\nclass Circle: Shape {\n  var radius: Double = 1\n  var diameter: Double { radius * 2 }\n  init(radius: Double) { self.radius = radius }\n  init() {}\n  deinit {}\n  subscript(i: Int) -> Int { i }\n  func area() -> Double { 0 }\n}\nclass View: UIView, Shape {}\nclass Source: NSObject, UITableViewDataSource {}\nclass Grid: UICollectionViewDiffableDataSource<Int, Int> {}\nstruct Box: Equatable { struct Inner {} }\nextension Box { func x() {} }\nextension Remote.Thing: Codable { func y() {} }\nenum Kind: Int, CaseIterable { case a }\n",
    );
    assert_eq!(prop(&sw, 0, "extraction_version"), Some("2.3"));
    let (shape_p, ..) = def(&sw, "Interface", "#type:Shape");
    assert_eq!(
        sw.nodes[shape_p].properties.get("class_bound"),
        Some(&serde_json::json!(true))
    );
    let drawable = sw
        .edges
        .iter()
        .find(|e| e.kind == "INHERITS" && e.source == shape_p)
        .unwrap();
    assert_eq!(drawable.target, sym("swift-type:Drawable"));
    assert_eq!(
        (drawable.resolution, drawable.confidence),
        (Resolution::Unresolved, 0.9)
    );
    assert_eq!(
        drawable.properties.get("swift_protocol_target"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(eprop(drawable, "origin_file"), Some("Sources/A.swift"));
    assert!(
        !sw.edges
            .iter()
            .any(|e| e.target == sym("swift-type:AnyObject")),
        "a class bound is not an edge"
    );
    let (req, ..) = def(&sw, "Method", "#type:Shape#method:area()");
    assert_eq!(prop(&sw, req, "argument_labels"), Some(""));
    let (name, ..) = def(&sw, "Property", "#type:Shape#property:name");
    assert_eq!(prop(&sw, name, "accessors"), Some("get"));
    assert_eq!(prop(&sw, name, "type"), Some("String"));
    assert!(local(&sw, "DEFINES", shape_p, name));

    let (circle, ..) = def(&sw, "Class", "#type:Circle");
    assert_eq!(prop(&sw, circle, "declaration_kind"), Some("class"));
    let conf = sw
        .edges
        .iter()
        .find(|e| e.kind == "IMPLEMENTS" && e.source == circle)
        .unwrap();
    assert_eq!(
        (conf.target.clone(), conf.resolution),
        (EdgeTarget::Local(shape_p), Resolution::Inferred),
        "a first entry naming a protocol of the file is a conformance, not a guess"
    );
    assert!(!conf.properties.contains_key("inheritance_inferred"));
    no_def(&sw, "#type:Circle#property:radius");
    let (dia, ..) = def(&sw, "Property", "#type:Circle#property:diameter");
    assert_eq!(
        sw.nodes[dia].properties.get("computed"),
        Some(&serde_json::json!(true))
    );
    let (init, ..) = def(&sw, "Method", "#type:Circle#method:init(radius:Double)");
    assert_eq!(prop(&sw, init, "argument_labels"), Some("radius:"));
    def(&sw, "Method", "#type:Circle#method:init()");
    def(&sw, "Method", "#type:Circle#method:deinit()");
    let (sub, ..) = def(&sw, "Method", "#type:Circle#method:subscript(i:Int)");
    assert!(sw.nodes[sub].properties.get("argument_labels").is_none());
    assert!(
        sw.nodes.iter().all(|n| n.kind != "Constructor"),
        "initializers are methods named init, as retained"
    );

    // A class's first entry not declared in the file: the naming heuristic, marked inferred.
    let first = |q: &str| {
        let (i, ..) = def(&sw, "Class", q);
        sw.edges
            .iter()
            .filter(|e| e.source == i && matches!(e.kind.as_str(), "INHERITS" | "IMPLEMENTS"))
            .map(|e| (e.kind.clone(), e.target.clone(), e.confidence))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        first("#type:View"),
        vec![
            ("INHERITS".to_owned(), sym("swift-type:UIView"), 0.6),
            ("IMPLEMENTS".to_owned(), EdgeTarget::Local(shape_p), 0.9),
        ]
    );
    assert_eq!(
        first("#type:Source"),
        vec![
            ("INHERITS".to_owned(), sym("swift-type:NSObject"), 0.6),
            (
                "IMPLEMENTS".to_owned(),
                sym("swift-type:UITableViewDataSource"),
                0.9
            ),
        ]
    );
    assert_eq!(
        first("#type:Grid"),
        vec![(
            "INHERITS".to_owned(),
            sym("swift-type:UICollectionViewDiffableDataSource"),
            0.6
        )],
        "a well-known framework class with a protocol-like name"
    );

    let (b, ..) = def(&sw, "Struct", "#type:Box");
    def(&sw, "Struct", "#type:Box#type:Inner");
    assert!(symbol(&sw, "IMPLEMENTS", b, "swift-type:Equatable").is_some());
    let (x, ..) = def(&sw, "Method", "#type:Box#method:x()");
    assert!(
        local(&sw, "DEFINES_METHOD", b, x),
        "members of a same-file extension attach to the extended type"
    );
    // An extension of a type declared elsewhere: members under `#extension:`, attached to the
    // type reference the indexer resolves.
    let (y, ..) = def(&sw, "Method", "#extension:Remote.Thing#method:y()");
    assert_eq!(prop(&sw, y, "receiver_owner"), Some("Thing"));
    let owner = sw
        .edges
        .iter()
        .find(|e| e.kind == "DEFINES_METHOD" && e.target == EdgeTarget::Local(y))
        .unwrap();
    assert_eq!(owner.source, 0);
    assert_eq!(
        eprop(owner, "source_qualified_name"),
        Some("swift-type:Remote.Thing")
    );
    assert_eq!(eprop(owner, "origin_file"), Some("Sources/A.swift"));
    let codable = sw
        .edges
        .iter()
        .find(|e| e.target == sym("swift-type:Codable"))
        .unwrap();
    assert_eq!((codable.kind.as_str(), codable.source), ("IMPLEMENTS", 0));
    assert_eq!(
        eprop(codable, "source_qualified_name"),
        Some("swift-type:Remote.Thing")
    );
    let (k, ..) = def(&sw, "Enum", "#type:Kind");
    assert_eq!(prop(&sw, k, "raw_type"), Some("Int"));
    assert!(symbol(&sw, "IMPLEMENTS", k, "swift-type:CaseIterable").is_some());
    assert!(symbol(&sw, "IMPLEMENTS", k, "swift-type:Int").is_none());

    // Imports: re-exports, scoped imports and submodules.
    let import = |m: &str| {
        sw.edges
            .iter()
            .find(|e| e.kind == "IMPORTS" && e.target == sym(m))
            .unwrap_or_else(|| panic!("import {m}"))
    };
    assert_eq!(
        import("Foundation").properties.get("exported"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(eprop(import("Geometry"), "symbol"), Some("Point"));
    assert_eq!(eprop(import("Geometry"), "symbol_kind"), Some("struct"));
    assert!(import("App.Core").properties.is_empty());
    assert_eq!(
        ext_imports(&sw),
        vec!["Geometry.Point".to_owned()],
        "the previous walk imported the written path"
    );

    // No Swift semantic tier: every extracted node is a tree-sitter fallback node.
    for n in &sw.nodes[1..] {
        assert_eq!(
            (
                n.properties.get("confidence").and_then(|v| v.as_str()),
                n.properties
                    .get("extractionSource")
                    .and_then(|v| v.as_str()),
                n.properties.get("fallbackReason").and_then(|v| v.as_str()),
            ),
            (
                Some("low"),
                Some("tree-sitter-fallback"),
                Some("swift_semantic_unavailable")
            ),
            "{}",
            n.qualified_name
        );
    }
}

#[test]
fn swift_call_sites_argument_labels_and_receivers() {
    let sw = extract(
        "Sources/Calls.swift",
        "class Circle {\n  var radius: Double = 1\n  var parent: Circle? = nil\n  init(radius: Double) {}\n  init() {}\n  func scale(by factor: Double, _ extra: Int = 0) {}\n  func helper(_ x: Int) -> Double { 0 }\n  func area() -> Double { 0 }\n  func run(completion: (Int) -> Void) {\n    scale(by: 2)\n    helper(1)\n    completion(1)\n    let c = Circle(radius: 2)\n    c.area()\n    Circle.init(radius: 3)\n    self.parent!.area()\n    unknown.area()\n    xs.forEach { item in item.area() }\n    apply(1) { } then: { }\n    for v in vs { v.area() }\n  }\n  static func make() -> Circle { return .init() }\n}\nfunc top() -> Circle { let k: Circle = .init(radius: 1); return k }\nextension Remote.Thing { func y() { y() } }\n",
    );
    let (circle, ..) = def(&sw, "Class", "#type:Circle");
    let (init_r, ..) = def(&sw, "Method", "#type:Circle#method:init(radius:Double)");
    let (init0, ..) = def(&sw, "Method", "#type:Circle#method:init()");
    let (scale, ..) = def(&sw, "Method", "#type:Circle#method:scale(by:Double,_:Int)");
    assert_eq!(prop(&sw, scale, "argument_labels"), Some("by:_=:"));
    let (helper, ..) = def(&sw, "Method", "#type:Circle#method:helper(_:Int)");
    let (area, ..) = def(&sw, "Method", "#type:Circle#method:area()");
    let (run, ..) = def(
        &sw,
        "Method",
        "#type:Circle#method:run(completion:(Int)->Void)",
    );
    assert_eq!(prop(&sw, run, "argument_labels"), Some("completion:"));
    let labels = |e: &ExtractedEdge| eprop(e, "argument_labels").map(str::to_owned);

    // A bare call of a method the enclosing type declares is a call on self; its labels pick
    // the overload a defaulted parameter allows.
    let e = call(&sw, run, EdgeTarget::Local(scale));
    assert_eq!(
        shape(e),
        (
            Resolution::Inferred,
            0.65,
            Some("Resolved"),
            Some("#type:Circle")
        )
    );
    assert_eq!(labels(e).as_deref(), Some("by:"));
    assert_eq!(
        labels(call(&sw, run, EdgeTarget::Local(helper))).as_deref(),
        Some("_:")
    );
    // Calling a closure parameter is not a call of a function of that name.
    assert_eq!(
        shape(call(&sw, run, sym("completion"))),
        (Resolution::Unresolved, 0.45, Some("Unresolved"), None)
    );
    // A call of a type is its init; the labels select the overload.
    let inits = calls(&sw, run, EdgeTarget::Local(init_r));
    assert_eq!(
        inits.len(),
        2,
        "`Circle(radius:)` and `Circle.init(radius:)`"
    );
    assert!(
        inits
            .iter()
            .all(|e| labels(e).as_deref() == Some("radius:") && e.confidence == 0.65)
    );
    assert!(calls(&sw, run, EdgeTarget::Local(init0)).is_empty());
    // Receivers proven by the file: a local initialized by a call of a type, an optional
    // property chain; others stay unresolved.
    let to_area = calls(&sw, run, EdgeTarget::Local(area));
    assert_eq!(to_area.len(), 2, "`c.area()` and `self.parent!.area()`");
    let unresolved: Vec<_> = calls(&sw, run, sym("area"))
        .into_iter()
        .map(|e| eprop(e, "receiver_kind"))
        .collect();
    assert_eq!(
        unresolved,
        vec![Some("Unresolved"); 3],
        "unknown, a closure parameter and a loop variable"
    );
    // Trailing closures: `*` for the first, the label of each later one.
    assert_eq!(
        labels(call(&sw, run, sym("apply"))).as_deref(),
        Some("_:*:then:")
    );
    assert_eq!(call(&sw, run, sym("apply")).confidence, 0.7);
    // An implicit member resolves against the contextual type: a return type, an annotation.
    let (make, ..) = def(&sw, "Method", "#type:Circle#method:make()");
    assert_eq!(
        shape(call(&sw, make, EdgeTarget::Local(init0))),
        (
            Resolution::Inferred,
            0.65,
            Some("Resolved"),
            Some("#type:Circle")
        )
    );
    let (top, ..) = def(&sw, "Function", "#function:top()");
    assert_eq!(call(&sw, top, EdgeTarget::Local(init_r)).confidence, 0.65);
    // A bare call inside a cross-file extension is a call on the extended type's reference.
    let (y, ..) = def(&sw, "Method", "#extension:Remote.Thing#method:y()");
    assert_eq!(
        shape(call(&sw, y, sym("y"))),
        (
            Resolution::Unresolved,
            0.65,
            Some("Resolved"),
            Some("swift-type:Remote.Thing")
        )
    );
    assert!(local(&sw, "DEFINES_METHOD", circle, run));
}

#[test]
fn baseline_calls_outside_retained_callables_are_extensions() {
    let x = extract(
        "tools/run.py",
        "import os, sys\n\nclass A:\n    registry = make()\n    def m(self, k=default()):\n        pass\n\ndef make():\n    return 1\n\ndef default():\n    return 2\n\nif __name__ == '__main__':\n    make()\n",
    );
    let (a, ..) = def(&x, "Class", "#type:A");
    let (m, ..) = def(&x, "Method", "#type:A#method:m(self,k=default())");
    let (make, ..) = def(&x, "Function", "#function:make()");
    let (default, ..) = def(&x, "Function", "#function:default()");
    // The retained walk records calls only in callable bodies.
    assert!(calls(&x, 0, EdgeTarget::Local(make)).is_empty());
    assert!(calls(&x, a, EdgeTarget::Local(make)).is_empty());
    assert!(calls(&x, m, EdgeTarget::Local(default)).is_empty());
    // The previous walk's calls: the module, a class body, a parameter's default value.
    assert_eq!(ext(&x, "CALLS", 0), vec![(EdgeTarget::Local(make), 0.7)]);
    assert_eq!(ext(&x, "CALLS", a), vec![(EdgeTarget::Local(make), 0.7)]);
    assert_eq!(ext(&x, "CALLS", m), vec![(EdgeTarget::Local(default), 0.7)]);
    assert_eq!(imports(&x), vec!["os".to_owned()]);
    assert_eq!(ext_imports(&x), vec!["sys".to_owned()]);
    // No node, label, qualified name or property is added by the extensions.
    assert_eq!(x.nodes.len(), 5);
}

#[test]
fn javascript_and_typescript_modules_classes_and_requires() {
    let js = extract(
        "web/app.js",
        "const x = require('./x');\nimport y from 'y';\nclass A extends B { m() { f(); this.g(); } g() {} }\nfunction f() {}\nconst k = () => f();\n",
    );
    let (a, ..) = def(&js, "Class", "#type:A");
    let (m, ..) = def(&js, "Method", "#type:A#method:m()");
    let (g, ..) = def(&js, "Method", "#type:A#method:g()");
    let (f, ..) = def(&js, "Function", "#function:f()");
    let (k, ..) = def(&js, "Function", "#function:k()");
    assert!(local(&js, "CALLS", m, f) && local(&js, "CALLS", m, g) && local(&js, "CALLS", k, f));
    assert!(symbol(&js, "INHERITS", a, "B").is_some());
    assert_eq!(imports(&js), vec!["./x".to_owned(), "y".into()]);

    let ts = extract(
        "web/c.ts",
        "import { a } from './a';\nexport class C extends D implements E { m(): void { this.n(); } n() {} }\ninterface E extends F {}\nenum K { A }\nnamespace NS { export function inner() {} }\n",
    );
    let (c, ..) = def(&ts, "Class", "#type:C");
    assert!(
        symbol(&ts, "INHERITS", c, "D").is_some() && symbol(&ts, "IMPLEMENTS", c, "E").is_some()
    );
    def(&ts, "Interface", "#type:E");
    def(&ts, "Enum", "#type:K");
    def(&ts, "Namespace", "#type:NS");
    def(&ts, "Function", "#type:NS#function:inner()");
    assert_eq!(imports(&ts), vec!["./a".to_owned()]);
    let tsx = extract(
        "web/v.tsx",
        "import React from 'react';\nexport function View() { return <div>{f()}</div>; }\nfunction f() { return 1; }\n",
    );
    let (v, ..) = def(&tsx, "Function", "#function:View()");
    let (f, ..) = def(&tsx, "Function", "#function:f()");
    assert!(local(&tsx, "CALLS", v, f));
    assert_eq!(tsx.parse_errors, 0);
}

#[test]
fn csharp_javascript_and_typescript_native_specs_keep_their_own_reading() {
    // C#: an alias or `static` using names the imported namespace; a generic call names the
    // method; interface bases are all IMPLEMENTS; a property has no signature.
    let cs = extract(
        "src/B.cs",
        "using Foo = Bar.Baz;\nusing static System.Console;\nnamespace A.B { interface IA : IB, IC {}\n class W { int P => 0; void G<T>() { x.Run<int>(); H<T>(); } void H<T>() {} } }\n",
    );
    assert_eq!(
        imports(&cs),
        vec!["Bar.Baz".to_owned(), "System.Console".into()]
    );
    let (ia, ..) = def(&cs, "Interface", "#type:A.B#type:IA");
    assert!(
        symbol(&cs, "IMPLEMENTS", ia, "IB").is_some()
            && symbol(&cs, "INHERITS", ia, "IB").is_none()
    );
    let (g, ..) = def(&cs, "Method", "#type:A.B#type:W#method:G()");
    let (h, ..) = def(&cs, "Method", "#type:A.B#type:W#method:H()");
    def(&cs, "Property", "#type:A.B#type:W#property:P");
    assert!(symbol(&cs, "CALLS", g, "Run").is_some());
    assert!(local(&cs, "CALLS", g, h));
    assert!(cs.edges.iter().all(|e| !is_ext(e)), "no baseline markers");

    // JavaScript and TypeScript: only a declarator bound to a function is a definition; `$` is
    // a callee; `require` imports whether or not it sits in a callable and is never a call;
    // `super()`, `a[0]()` and a bare string export are neither calls nor imports.
    let js = extract(
        "web/m.ts",
        "const n = 5, { d } = pair();\nconst f = (x: number): string => $(x);\nexport const r = require('r');\nexport default 'use strict';\nfunction g() { const q = require(\"q\"); a[0](); }\nclass A extends B { constructor() { super(); } }\nthis.top();\n",
    );
    no_def(&js, "#function:n()");
    no_def(&js, "#function:d()");
    let (f, ..) = def(&js, "Function", "#function:f(x:number)");
    assert_eq!(prop(&js, f, "return_type"), Some(": string"));
    assert!(symbol(&js, "CALLS", f, "$").is_some());
    assert_eq!(imports(&js), vec!["q".to_owned(), "r".into()]);
    assert!(symbol(&js, "CALLS", 0, "require").is_none());
    assert!(
        js.edges
            .iter()
            .all(|e| e.kind != "CALLS"
                || !matches!(&e.target, EdgeTarget::Symbol(s) if s == "super"))
    );
    assert_eq!(
        symbol(&js, "CALLS", 0, "top"),
        Some((Resolution::Unresolved, 0.45))
    );
    assert!(js.edges.iter().all(|e| !is_ext(e)), "no baseline markers");

    // Names keep their spelling (`#f`, `@Foo`, nested generics); a default parameter's
    // definitions are definitions.
    let k = extract(
        "web/k.ts",
        "class A { #f() {} f() {} m() { this.#f(); } }\nfunction á() {}\nfunction b() { á(); }\nfunction outer(cb = () => { function inner() {} inner(); }) {}\n",
    );
    let (hf, ..) = def(&k, "Method", "#type:A#method:#f()");
    let (m, ..) = def(&k, "Method", "#type:A#method:m()");
    assert!(local(&k, "CALLS", m, hf));
    let (accent, ..) = def(&k, "Function", "#function:á()");
    let (b, ..) = def(&k, "Function", "#function:b()");
    assert!(local(&k, "CALLS", b, accent));
    let (inner, ..) = def(
        &k,
        "Function",
        "#function:outer(cb=()=>{function inner(){}inner();})#function:inner()",
    );
    assert!(
        k.edges
            .iter()
            .any(|e| e.kind == "CALLS" && e.target == EdgeTarget::Local(inner)),
        "the call binds to the nested definition"
    );
    let cs = extract(
        "src/C.cs",
        "class C { void Run<T>() {} void @Foo() {} void M() { Run<List<int>>(); @Foo(); } }\n",
    );
    let (m, ..) = def(&cs, "Method", "#type:C#method:M()");
    let (run, ..) = def(&cs, "Method", "#type:C#method:Run()");
    let (foo, ..) = def(&cs, "Method", "#type:C#method:@Foo()");
    assert!(local(&cs, "CALLS", m, run) && local(&cs, "CALLS", m, foo));
}

#[test]
fn sql_terraform_ansible_and_coldfusion_domain_structure() {
    // The retained CodeGraph domain extractors (golden detail in tests/domain.rs): qualified names
    // are the retained ones without the repository prefix.
    let sql = extract(
        "db/schema.sql",
        "CREATE TABLE users (id INT PRIMARY KEY, org_id INT REFERENCES orgs(id), name TEXT);\nCREATE VIEW active AS SELECT * FROM users;\nCREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql;\n",
    );
    let (t, ..) = def(&sql, "Table", "users");
    assert!(
        sql.nodes.iter().all(|n| n.kind != "Column"),
        "columns are a property of the table"
    );
    assert!(
        symbol(&sql, "QUERIES", t, "orgs").is_some(),
        "a foreign key"
    );
    let (v, ..) = def(&sql, "View", "active");
    assert!(local(&sql, "QUERIES", v, t));
    def(&sql, "Function", "f");
    assert_eq!(sql.parse_errors, 0);

    let tf = extract(
        "infra/main.tf",
        "resource \"aws_s3_bucket\" \"b\" {\n  bucket = var.name\n}\nmodule \"net\" {\n  source = \"./net\"\n}\nvariable \"name\" {}\noutput \"id\" { value = aws_s3_bucket.b.id }\ndata \"aws_ami\" \"u\" {}\n",
    );
    let (b, ..) = def(&tf, "TerraformResource", "resource.aws_s3_bucket.b");
    let (m, ..) = def(&tf, "TerraformModule", "module.net");
    def(&tf, "TerraformVariable", "var.name");
    let (o, ..) = def(&tf, "TerraformOutput", "output.id");
    def(&tf, "TerraformDataSource", "data.aws_ami.u");
    assert!(symbol(&tf, "INCLUDES_MODULE", m, "module_source.net").is_some());
    assert!(
        !local(&tf, "CALLS", o, b),
        "the retained reference pattern needs two-character names: `aws_s3_bucket.b` is missed"
    );
    assert!(
        symbol(&tf, "CONFIGURES", b, "var.name").is_some(),
        "the bucket"
    );

    let yml = extract(
        "site.yml",
        "- name: web\n  hosts: all\n  roles:\n    - common\n    - role: nginx\n  tasks:\n    - name: install\n      apt: name=nginx\n      notify: restart\n  handlers:\n    - name: restart\n      service: name=nginx state=restarted\n",
    );
    let (play, ..) = def(&yml, "Playbook", "playbook.site");
    let (task, ..) = def(&yml, "AnsibleTask", "playbook.site.task.install");
    let (handler, ..) = def(&yml, "AnsibleHandler", "handler.restart");
    assert!(local(&yml, "DEFINES", play, task));
    let (common, ..) = def(&yml, "Role", "role.common");
    let (nginx, ..) = def(&yml, "Role", "role.nginx");
    assert!(
        local(&yml, "INCLUDES_ROLE", play, common) && local(&yml, "INCLUDES_ROLE", play, nginx)
    );
    assert!(local(&yml, "NOTIFIES_HANDLER", task, handler));
    assert!(
        symbol(&yml, "CALLS", play, "all").is_some(),
        "the play's hosts"
    );
    let plain = extract("config.yaml", "a: 1\nb: [2, 3]\n");
    assert_eq!(plain.nodes.len(), 1, "non-Ansible YAML: its file node");

    let cfc = extract(
        "app/User.cfc",
        "<cfcomponent>\n<cffunction name=\"getUser\" access=\"public\">\n<cfreturn 1>\n</cffunction>\n</cfcomponent>\n",
    );
    let (u, ..) = def(&cfc, "Component", "User");
    let (g, ..) = def(&cfc, "Function", "User.getUser");
    assert!(local(&cfc, "DEFINES_METHOD", u, g));
    let script = extract(
        "app/Svc.cfc",
        "component { function load() { return helper(1); } function helper(x) { return x; } }\n",
    );
    let (s, ..) = def(&script, "Component", "Svc");
    let (l, ..) = def(&script, "Function", "Svc.load");
    let (h, ..) = def(&script, "Function", "Svc.helper");
    assert!(local(&script, "DEFINES_METHOD", s, l) && local(&script, "DEFINES_METHOD", s, h));
    assert!(
        !local(&script, "CALLS", l, h),
        "the retained ColdFusion parser records no calls between functions"
    );
    no_def(&script, "Svc.missing");
}

#[test]
fn deep_and_large_files_stay_bounded() {
    // Deep nesting is walked without recursion.
    let deep = format!("fn f() {}{}\n", "{".repeat(5000), "}".repeat(5000));
    let x = extract("deep.rs", &deep);
    assert!(x.nodes.len() >= 2);
    // More definitions than the per-file bound: kept partial and marked.
    let many: String = (0..12_000).map(|i| format!("fn f{i}() {{}}\n")).collect();
    let p = TreeSitterParser::new();
    let x = p
        .extract(
            "rust",
            "many.rs",
            "repo",
            many.as_bytes(),
            &|| false,
            60_000,
        )
        .unwrap();
    assert!(x.nodes.len() <= 10_000);
    assert!(x.parse_errors > 0, "a truncated extraction is partial");
    assert_eq!(
        x.nodes[0]
            .properties
            .get("truncated")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}
