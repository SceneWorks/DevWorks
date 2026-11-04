//! Tree-sitter structural extraction golden inputs (CODE-01/02, CAP-LANGUAGES): every language
//! of the retained LanguageSpecs and the frozen adapter surface with a grammar, asserting exact
//! definitions, spans, containment, imports, calls and inheritance (positive and negative), the
//! honest text fallback, cancellation and the per-file budget. No inference.
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, FileExtract, LanguageMode, Resolution, StructuralParser,
};

fn extract(path: &str, src: &str) -> FileExtract {
    let p = TreeSitterParser::new();
    let lang = p.language_of(path).expect("a language").id;
    p.extract(lang, path, src.as_bytes(), &|| false, 5_000)
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
fn imports(x: &FileExtract) -> Vec<String> {
    let mut v: Vec<String> = x
        .edges
        .iter()
        .filter(|e| e.kind == "IMPORTS")
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
        p.extract("kotlin", "a.kt", b"fun x() {}", &|| false, 100),
        Err(ExtractFault::Failed(_))
    ));
    assert!(
        matches!(
            p.extract("rust", "a.rs", b"fn a() {}\0\0", &|| false, 100),
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
        p.extract("rust", "big.rs", big.as_bytes(), &|| true, 60_000),
        Err(ExtractFault::Cancelled)
    );
    assert!(matches!(
        p.extract("rust", "big.rs", big.as_bytes(), &|| false, 1),
        Err(ExtractFault::TimedOut) | Ok(_)
    ));
}

#[test]
fn rust_definitions_impls_calls_imports_and_abi() {
    let x = extract(
        "src/lib.rs",
        "use crate::a::b;\nmod m { pub fn inner() {} }\npub struct S;\npub trait T {}\nimpl T for S {\n    fn f(&self) {\n        helper();\n        self.g();\n        other.h();\n    }\n    fn g(&self) {}\n}\nfn helper() {}\n#[no_mangle]\npub extern \"C\" fn exported() {}\nextern \"C\" fn callback() {}\n",
    );
    let (s, l, _) = def(&x, "Struct", "S");
    assert_eq!(l, 3);
    let (t, ..) = def(&x, "Interface", "T");
    let (f, fl, fe) = def(&x, "Method", "S.f");
    assert_eq!((fl, fe), (6, 10));
    let (g, ..) = def(&x, "Method", "S.g");
    let (h, ..) = def(&x, "Function", "helper");
    def(&x, "Module", "m");
    def(&x, "Function", "m.inner");
    assert!(local(&x, "DEFINES", 0, s) && local(&x, "DEFINES", 0, t));
    assert!(
        local(&x, "DEFINES_METHOD", s, f),
        "impl members attach to the local type"
    );
    assert!(symbol(&x, "IMPLEMENTS", s, "T").is_some());
    let bare = x
        .edges
        .iter()
        .find(|e| e.kind == "CALLS" && e.source == f && e.target == EdgeTarget::Local(h))
        .unwrap();
    assert_eq!(
        (bare.resolution, bare.confidence),
        (Resolution::Inferred, 0.7)
    );
    let on_self = x
        .edges
        .iter()
        .find(|e| e.kind == "CALLS" && e.source == f && e.target == EdgeTarget::Local(g))
        .unwrap();
    assert_eq!(on_self.confidence, 0.65);
    assert_eq!(
        symbol(&x, "CALLS", f, "h"),
        Some((Resolution::Unresolved, 0.45))
    );
    assert_eq!(imports(&x), vec!["crate::a::b".to_owned()]);
    let (e, ..) = def(&x, "Function", "exported");
    assert_eq!(
        x.nodes[e].properties.get("abi").and_then(|v| v.as_str()),
        Some("C")
    );
    let (cb, ..) = def(&x, "Function", "callback");
    assert!(
        x.nodes[cb].properties.get("abi").is_none(),
        "an unexported extern fn keeps no symbol ABI"
    );
    assert_eq!(x.parse_errors, 0);
}

#[test]
fn python_classes_bases_imports_and_calls() {
    let x = extract(
        "pkg/mod.py",
        "import os, a.b as c\nfrom x.y import z\n\nclass A(Base, Mixin):\n    def m(self):\n        self.n()\n        f()\n    def n(self):\n        pass\n\ndef f():\n    os.path.join('a')\n",
    );
    let (a, l, e) = def(&x, "Class", "A");
    assert_eq!((l, e), (4, 9));
    let (m, ..) = def(&x, "Method", "A.m");
    let (n, ..) = def(&x, "Method", "A.n");
    let (f, ..) = def(&x, "Function", "f");
    assert!(
        symbol(&x, "INHERITS", a, "Base").is_some() && symbol(&x, "INHERITS", a, "Mixin").is_some()
    );
    assert!(local(&x, "CALLS", m, n) && local(&x, "CALLS", m, f));
    assert_eq!(
        symbol(&x, "CALLS", f, "join").map(|s| s.0),
        Some(Resolution::Unresolved)
    );
    assert_eq!(
        imports(&x),
        vec!["a.b".to_owned(), "os".into(), "x.y".into()]
    );
}

#[test]
fn c_and_cpp_definitions_includes_namespaces_and_linkage() {
    let c = extract(
        "src/main.c",
        "#include <stdio.h>\n#include \"util.h\"\nstruct S { int a; };\nstruct S *decl;\ntypedef struct { int b; } T;\nenum E { X };\nstatic int helper(void) { return 1; }\nint main(void) { helper(); printf(\"x\"); return 0; }\n",
    );
    def(&c, "Struct", "S");
    def(&c, "Struct", "T");
    def(&c, "Enum", "E");
    let (h, ..) = def(&c, "Function", "helper");
    let (m, ml, _) = def(&c, "Function", "main");
    assert_eq!(ml, 8);
    assert!(local(&c, "CALLS", m, h));
    assert_eq!(imports(&c), vec!["stdio.h".to_owned(), "util.h".into()]);
    assert_eq!(
        c.nodes.iter().filter(|n| n.name == "S").count(),
        1,
        "a declaration without a body is no definition"
    );

    let cpp = extract(
        "src/a.cpp",
        "#include \"a.h\"\nnamespace n {\nclass C : public B { void m(); };\n}\nvoid n::C::m() { f(); this->g(); }\nextern \"C\" int e() { return 0; }\nint f() { return 1; }\n",
    );
    let (ns, ..) = def(&cpp, "Namespace", "n");
    let (cls, ..) = def(&cpp, "Class", "n.C");
    assert!(local(&cpp, "DEFINES", ns, cls));
    assert!(symbol(&cpp, "INHERITS", cls, "B").is_some());
    let (m, ..) = def(&cpp, "Method", "C.m");
    let (f, ..) = def(&cpp, "Function", "f");
    assert!(local(&cpp, "CALLS", m, f));
    let (e, ..) = def(&cpp, "Function", "e");
    assert_eq!(
        cpp.nodes[e].properties.get("abi").and_then(|v| v.as_str()),
        Some("C")
    );
}

#[test]
fn csharp_java_and_go_types_members_and_inheritance() {
    let cs = extract(
        "src/A.cs",
        "using System.Text;\nnamespace N {\n  public class A : Base, IThing {\n    public A() {}\n    public void M() { Foo(); x.Bar(); }\n    public int P { get; set; }\n    void Foo() {}\n  }\n  interface IThing {}\n  record R(int X);\n}\n",
    );
    let (a, ..) = def(&cs, "Class", "N.A");
    def(&cs, "Constructor", "N.A.A");
    let (m, ..) = def(&cs, "Method", "N.A.M");
    let (foo, ..) = def(&cs, "Method", "N.A.Foo");
    def(&cs, "Property", "N.A.P");
    def(&cs, "Interface", "N.IThing");
    def(&cs, "Record", "N.R");
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

    let java = extract(
        "src/A.java",
        "import a.b.C;\nclass A extends B implements I, J { A() {} void m() { f(); x.g(); } void f() {} }\ninterface I extends K {}\nenum E { X }\n",
    );
    let (a, ..) = def(&java, "Class", "A");
    def(&java, "Constructor", "A.A");
    let (m, ..) = def(&java, "Method", "A.m");
    let (f, ..) = def(&java, "Method", "A.f");
    assert!(local(&java, "CALLS", m, f));
    assert!(symbol(&java, "INHERITS", a, "B").is_some());
    assert!(
        symbol(&java, "IMPLEMENTS", a, "I").is_some()
            && symbol(&java, "IMPLEMENTS", a, "J").is_some()
    );
    def(&java, "Interface", "I");
    def(&java, "Enum", "E");
    assert_eq!(imports(&java), vec!["a.b.C".to_owned()]);

    let go = extract(
        "pkg/s.go",
        "package p\nimport (\n\t\"fmt\"\n\tx \"a/b\"\n)\ntype S struct{}\ntype I interface{}\nfunc (s *S) M() { fmt.Println(); f() }\nfunc f() {}\n",
    );
    let (s, ..) = def(&go, "Struct", "S");
    def(&go, "Interface", "I");
    let (m, ..) = def(&go, "Method", "S.M");
    let (f, ..) = def(&go, "Function", "f");
    assert!(
        local(&go, "DEFINES_METHOD", s, m),
        "a Go method attaches to its receiver type"
    );
    assert!(local(&go, "CALLS", m, f));
    assert_eq!(imports(&go), vec!["a/b".to_owned(), "fmt".into()]);
}

#[test]
fn ruby_php_bash_and_swift() {
    let rb = extract(
        "lib/a.rb",
        "require 'json'\nrequire_relative 'util'\nclass A < B\n  def m\n    foo(1)\n    x.bar\n  end\n  def foo(x); end\nend\nmodule M; end\n",
    );
    let (a, ..) = def(&rb, "Class", "A");
    let (m, ..) = def(&rb, "Method", "A.m");
    let (foo, ..) = def(&rb, "Method", "A.foo");
    def(&rb, "Module", "M");
    assert!(symbol(&rb, "INHERITS", a, "B").is_some());
    assert!(local(&rb, "CALLS", m, foo));
    assert_eq!(imports(&rb), vec!["json".to_owned(), "util".into()]);

    let php = extract(
        "src/a.php",
        "<?php\nuse A\\B;\nclass C extends D implements E { function m() { f(); $this->g(); X::y(); } function g() {} }\nfunction f() {}\n",
    );
    let (c, ..) = def(&php, "Class", "C");
    let (m, ..) = def(&php, "Method", "C.m");
    let (g, ..) = def(&php, "Method", "C.g");
    def(&php, "Function", "f");
    assert!(local(&php, "CALLS", m, g));
    assert!(
        symbol(&php, "INHERITS", c, "D").is_some() && symbol(&php, "IMPLEMENTS", c, "E").is_some()
    );
    assert_eq!(symbol(&php, "CALLS", m, "X.y").map(|s| s.1), Some(0.45));

    let sh = extract(
        "bin/run.sh",
        "source ./lib.sh\nf() { echo hi; }\nfunction g { f; }\ng\nls -la\n",
    );
    let (f, ..) = def(&sh, "Function", "f");
    let (g, ..) = def(&sh, "Function", "g");
    assert!(local(&sh, "CALLS", g, f));
    assert!(local(&sh, "CALLS", 0, g));
    assert!(
        !sh.edges
            .iter()
            .any(|e| e.target == EdgeTarget::Symbol("ls".into())),
        "programs are not calls"
    );
    assert_eq!(imports(&sh), vec!["./lib.sh".to_owned()]);

    let sw = extract(
        "Sources/A.swift",
        "import Foundation\nprotocol P: AnyObject {}\nclass A: UIView, P {\n  func f() { g() }\n  func g() {}\n  init() {}\n}\nstruct S: Equatable {}\nextension S { func x() {} }\nclass Q: P {}\n",
    );
    let (a, ..) = def(&sw, "Class", "A");
    let (p, ..) = def(&sw, "Interface", "P");
    assert!(
        sw.edges
            .iter()
            .all(|e| e.source != p || e.target != EdgeTarget::Symbol("AnyObject".into())),
        "a class bound is not an edge"
    );
    let inherits = sw
        .edges
        .iter()
        .find(|e| e.kind == "INHERITS" && e.source == a)
        .unwrap();
    assert!(inherits.properties.contains_key("inheritance_inferred"));
    assert!(symbol(&sw, "IMPLEMENTS", a, "P").is_some());
    let (s, ..) = def(&sw, "Struct", "S");
    assert!(symbol(&sw, "IMPLEMENTS", s, "Equatable").is_some());
    let (x, ..) = def(&sw, "Method", "S.x");
    assert!(
        local(&sw, "DEFINES_METHOD", s, x),
        "extension members attach to the extended type"
    );
    def(&sw, "Constructor", "A.init");
    // A first entry naming a protocol of the file is a conformance, not a guess.
    let (q, ..) = def(&sw, "Class", "Q");
    let conf = sw
        .edges
        .iter()
        .find(|e| e.kind == "IMPLEMENTS" && e.source == q)
        .unwrap();
    assert!(!conf.properties.contains_key("inheritance_inferred"));
    assert_eq!(imports(&sw), vec!["Foundation".to_owned()]);
}

#[test]
fn javascript_and_typescript_modules_classes_and_requires() {
    let js = extract(
        "web/app.js",
        "const x = require('./x');\nimport y from 'y';\nclass A extends B { m() { f(); this.g(); } g() {} }\nfunction f() {}\nconst k = () => f();\n",
    );
    let (a, ..) = def(&js, "Class", "A");
    let (m, ..) = def(&js, "Method", "A.m");
    let (g, ..) = def(&js, "Method", "A.g");
    let (f, ..) = def(&js, "Function", "f");
    let (k, ..) = def(&js, "Function", "k");
    assert!(local(&js, "CALLS", m, f) && local(&js, "CALLS", m, g) && local(&js, "CALLS", k, f));
    assert!(symbol(&js, "INHERITS", a, "B").is_some());
    assert_eq!(imports(&js), vec!["./x".to_owned(), "y".into()]);

    let ts = extract(
        "web/c.ts",
        "import { a } from './a';\nexport class C extends D implements E { m(): void { this.n(); } n() {} }\ninterface E extends F {}\nenum K { A }\nnamespace NS { export function inner() {} }\n",
    );
    let (c, ..) = def(&ts, "Class", "C");
    assert!(
        symbol(&ts, "INHERITS", c, "D").is_some() && symbol(&ts, "IMPLEMENTS", c, "E").is_some()
    );
    def(&ts, "Interface", "E");
    def(&ts, "Enum", "K");
    def(&ts, "Namespace", "NS");
    def(&ts, "Function", "NS.inner");
    assert_eq!(imports(&ts), vec!["./a".to_owned()]);
    let tsx = extract(
        "web/v.tsx",
        "import React from 'react';\nexport function View() { return <div>{f()}</div>; }\nfunction f() { return 1; }\n",
    );
    let (v, ..) = def(&tsx, "Function", "View");
    let (f, ..) = def(&tsx, "Function", "f");
    assert!(local(&tsx, "CALLS", v, f));
    assert_eq!(tsx.parse_errors, 0);
}

#[test]
fn sql_terraform_ansible_and_coldfusion_domain_structure() {
    let sql = extract(
        "db/schema.sql",
        "CREATE TABLE users (id INT PRIMARY KEY, org_id INT REFERENCES orgs(id), name TEXT);\nCREATE VIEW active AS SELECT * FROM users;\nCREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql;\n",
    );
    let (t, ..) = def(&sql, "Table", "users");
    let (col, ..) = def(&sql, "Column", "users.org_id");
    assert!(local(&sql, "HAS_COLUMN", t, col));
    assert!(symbol(&sql, "FOREIGN_KEY", t, "orgs").is_some());
    let (v, ..) = def(&sql, "View", "active");
    assert!(symbol(&sql, "QUERIES", v, "users").is_some());
    def(&sql, "Function", "f");

    let tf = extract(
        "infra/main.tf",
        "resource \"aws_s3_bucket\" \"b\" {\n  bucket = var.name\n}\nmodule \"net\" {\n  source = \"./net\"\n}\nvariable \"name\" {}\noutput \"id\" { value = aws_s3_bucket.b.id }\ndata \"aws_ami\" \"u\" {}\n",
    );
    let (b, ..) = def(&tf, "TerraformResource", "aws_s3_bucket.b");
    let (m, ..) = def(&tf, "TerraformModule", "module.net");
    def(&tf, "TerraformVariable", "var.name");
    let (o, ..) = def(&tf, "TerraformOutput", "output.id");
    def(&tf, "TerraformDataSource", "data.aws_ami.u");
    assert!(symbol(&tf, "INCLUDES_MODULE", m, "./net").is_some());
    assert!(symbol(&tf, "DEPENDS_ON", o, "aws_s3_bucket.b").is_some());
    assert!(symbol(&tf, "DEPENDS_ON", b, "var.name").is_some());

    let yml = extract(
        "site.yml",
        "- name: web\n  hosts: all\n  roles:\n    - common\n    - role: nginx\n  tasks:\n    - name: install\n      apt: name=nginx\n      notify: restart\n  handlers:\n    - name: restart\n      service: name=nginx state=restarted\n",
    );
    let (play, ..) = def(&yml, "Playbook", "web");
    let (task, ..) = def(&yml, "AnsibleTask", "web.install");
    def(&yml, "AnsibleHandler", "web.restart");
    assert!(local(&yml, "DEFINES", play, task));
    assert!(
        symbol(&yml, "INCLUDES_ROLE", play, "common").is_some()
            && symbol(&yml, "INCLUDES_ROLE", play, "nginx").is_some()
    );
    assert!(symbol(&yml, "NOTIFIES_HANDLER", task, "restart").is_some());
    let plain = extract("config.yaml", "a: 1\nb: [2, 3]\n");
    assert_eq!(
        plain.nodes.len(),
        1,
        "plain YAML is syntax-only: its file node"
    );

    let cfc = extract(
        "app/User.cfc",
        "<cfcomponent>\n<cffunction name=\"getUser\" access=\"public\">\n<cfreturn 1>\n</cffunction>\n</cfcomponent>\n",
    );
    let (u, ..) = def(&cfc, "Class", "User");
    let (g, ..) = def(&cfc, "Method", "User.getUser");
    assert!(local(&cfc, "DEFINES_METHOD", u, g));
    let script = extract(
        "app/Svc.cfc",
        "component { function load() { return helper(1); } function helper(x) { return x; } }\n",
    );
    let (l, ..) = def(&script, "Method", "Svc.load");
    let (h, ..) = def(&script, "Method", "Svc.helper");
    assert!(local(&script, "CALLS", l, h));
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
        .extract("rust", "many.rs", many.as_bytes(), &|| false, 60_000)
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
