//! Backend endpoint registrations (Axum, Gin) and manifest package identities (Cargo, SwiftPM,
//! Xcode, NuGet, Go modules): the per-file and group extraction outputs, exactly as retained.
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_core::ports::indexing::{EdgeTarget, FileExtract, StructuralParser};
use serde_json::{Value, json};

fn extract(path: &str, src: &str) -> FileExtract {
    let p = TreeSitterParser::new();
    let lang = p.language_of(path).expect("a language").id;
    p.extract(lang, path, "repo", src.as_bytes(), &|| false, 5_000)
        .expect("extracts")
}
fn routes(x: &FileExtract) -> Vec<(String, Value)> {
    x.nodes
        .iter()
        .filter(|n| n.kind == "Route")
        .map(|n| (n.name.clone(), Value::Object(n.properties.clone())))
        .collect()
}
fn handles(x: &FileExtract, route: &str) -> Option<(String, Value)> {
    let i = x
        .nodes
        .iter()
        .position(|n| n.kind == "Route" && n.name == route)?;
    x.edges
        .iter()
        .find(|e| e.kind == "HANDLES" && e.source == i)
        .map(|e| match &e.target {
            EdgeTarget::Symbol(s) => (s.clone(), Value::Object(e.properties.clone())),
            other => panic!("{other:?}"),
        })
}

#[test]
fn axum_registrations_become_routes_with_scoped_handlers() {
    let src = r#"
use axum::{routing::{get, post}, Router};
mod handlers;

fn users() -> Router {
    Router::new().route("/users", get(list).post(handlers::create))
}

pub fn app() -> Router {
    let api = Router::new().route("/health", get(health)).route("/any", axum::routing::any(fallback));
    Router::new().nest("/api", api).merge(users())
}

fn list() {}
fn health() {}
fn fallback() {}
"#;
    let x = extract("src/lib.rs", src);
    let names: Vec<String> = routes(&x).into_iter().map(|(n, _)| n).collect();
    for want in [
        "GET /api/health",
        "GET /users",
        "POST /users",
        "GET /api/any",
        "PUT /api/any",
    ] {
        assert!(names.contains(&want.to_owned()), "{want}: {names:?}");
    }
    let (_, props) = routes(&x)
        .into_iter()
        .find(|(n, _)| n == "POST /users")
        .unwrap();
    assert_eq!(props["framework"], json!("axum"));
    assert_eq!(props["route_template"], json!("/users"));
    assert_eq!(props["handler_expression"], json!("handlers::create"));
    // A bare handler is looked up in this file at its definition lines.
    let (name, scope) = handles(&x, "GET /users").unwrap();
    assert_eq!(name, "list");
    assert_eq!(scope["candidateUnits"], json!(["src/lib.rs"]));
    assert_eq!(scope["candidateLines"]["src/lib.rs"], json!([14]));
    // A module path is looked up in that module's files.
    let (name, scope) = handles(&x, "POST /users").unwrap();
    assert_eq!(name, "create");
    let units = scope["candidateUnits"].as_array().unwrap();
    assert!(
        units.contains(&json!("src/handlers.rs")) && units.contains(&json!("src/handlers/mod.rs"))
    );
    // `any` registers every method as the fallback, marked so.
    let (_, any) = routes(&x)
        .into_iter()
        .find(|(n, _)| n == "PUT /api/any")
        .unwrap();
    assert_eq!(any["registration_method"], json!("ANY"));
}

#[test]
fn axum_explicit_methods_win_over_the_any_fallback_and_errors_claim_nothing() {
    let src = r#"
use axum::{routing::{any, get}, Router};
fn app() -> Router { Router::new().route("/x", get(a).merge_any()).route("/x", any(b)) }
fn a() {} fn b() {}
"#;
    let x = extract("src/main.rs", src);
    let gets: Vec<(String, Value)> = routes(&x)
        .into_iter()
        .filter(|(n, _)| n == "GET /x")
        .collect();
    assert_eq!(gets.len(), 1, "{gets:?}");
    let broken = extract(
        "src/main.rs",
        "use axum::Router;\nfn app() { Router::new().route(\"/\", get(h) }\n",
    );
    assert!(routes(&broken).is_empty());
    assert!(
        broken
            .incomplete
            .iter()
            .any(|w| w.starts_with("backend-endpoints"))
    );
    // No Axum import: no registrations even with a `.route(` call.
    let other = extract("src/x.rs", "fn f(r: R) { r.route(\"/\", g()) }\n");
    assert!(routes(&other).is_empty() && other.incomplete.is_empty());
}

#[test]
fn gin_registrations_follow_groups_and_package_handlers() {
    let src = r#"package api

import (
    "github.com/gin-gonic/gin"
    h "example.com/svc/internal/handlers"
)

func Register(r *gin.Engine) {
    v1 := r.Group("/v1")
    v1.GET("/users/:id", h.GetUser)
    v1.Any("/ping", ping)
    r.Handle("PATCH", "/items", patchItem)
}

func ping(c *gin.Context) {}
"#;
    let x = extract("api/routes.go", src);
    let names: Vec<String> = routes(&x).into_iter().map(|(n, _)| n).collect();
    for want in [
        "GET /v1/users/:id",
        "POST /v1/ping",
        "TRACE /v1/ping",
        "PATCH /items",
    ] {
        assert!(names.contains(&want.to_owned()), "{want}: {names:?}");
    }
    let (name, scope) = handles(&x, "GET /v1/users/:id").unwrap();
    assert_eq!(name, "GetUser");
    assert_eq!(
        scope["goImport"],
        json!("example.com/svc/internal/handlers")
    );
    let (name, scope) = handles(&x, "POST /v1/ping").unwrap();
    assert_eq!(name, "ping");
    assert_eq!(scope["candidateDirs"], json!(["api"]));
    let (_, props) = routes(&x)
        .into_iter()
        .find(|(n, _)| n == "GET /v1/users/:id")
        .unwrap();
    assert_eq!(props["framework"], json!("gin"));
    assert_eq!(
        props["handler_scope"],
        json!("example.com/svc/internal/handlers")
    );
}

fn group(
    name: &str,
    repo: &str,
    files: &[(&str, &str)],
) -> std::collections::BTreeMap<String, FileExtract> {
    let p = TreeSitterParser::new();
    let files: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(a, b)| ((*a).to_owned(), b.as_bytes().to_vec()))
        .collect();
    for (f, _) in &files {
        assert_eq!(p.group_of(f), Some(name), "{f}");
    }
    p.extract_group(name, repo, &files, &|| false)
        .expect("group")
}
fn node<'a>(
    x: &'a FileExtract,
    kind: &str,
    qn: &str,
) -> (usize, &'a serde_json::Map<String, Value>) {
    x.nodes
        .iter()
        .enumerate()
        .find(|(_, n)| n.kind == kind && n.qualified_name == qn)
        .map(|(i, n)| (i, &n.properties))
        .unwrap_or_else(|| {
            panic!(
                "no {kind} {qn}: {:?}",
                x.nodes
                    .iter()
                    .map(|n| &n.qualified_name)
                    .collect::<Vec<_>>()
            )
        })
}

#[test]
fn cargo_workspaces_define_packages_and_resolve_path_and_inherited_dependencies() {
    let root = r#"
[workspace]
members = ["core", "app"]
[workspace.package]
version = "0.4.0"
publish = false
[workspace.dependencies]
serde = { version = "1.0.200", features = ["derive"] }
"#;
    let core = r#"
[package]
name = "acme-core"
version.workspace = true
publish.workspace = true
[dependencies]
serde = { workspace = true, optional = true }
"#;
    let app = r#"
[package]
name = "acme-app"
version = "1.2.3"
[dependencies]
acme-core = { path = "../core" }
sibling = { path = "../../sibling" }
tokio = { git = "https://github.com/tokio-rs/tokio.git", branch = "main" }
[target.'cfg(unix)'.dev-dependencies]
libc = "0.2"
"#;
    let out = group(
        "cargo",
        "acme",
        &[
            ("Cargo.toml", root),
            ("core/Cargo.toml", core),
            ("app/Cargo.toml", app),
        ],
    );
    let c = &out["core/Cargo.toml"];
    // Inherited version and `publish = false`: a workspace package.
    let (ci, cp) = node(c, "Package", "package:cargo:workspace:acme:acme-core@0.4.0");
    assert_eq!(cp["is_definition"], json!(true));
    let (si, sp) = node(
        c,
        "Package",
        "package:cargo:registry:crates.io:serde@1.0.200",
    );
    assert_eq!(
        (sp["workspace_inherited"].clone(), sp["optional"].clone()),
        (json!(true), json!(true))
    );
    assert!(c.edges.iter().any(|e| e.kind == "REFERENCES_PACKAGE"
        && e.source == ci
        && e.target == EdgeTarget::Local(si)));
    let a = &out["app/Cargo.toml"];
    let (ai, _) = node(
        a,
        "Package",
        "package:cargo:registry:crates.io:acme-app@1.2.3",
    );
    // The in-repository path dependency points at the other manifest's definition exactly.
    assert!(a.edges.iter().any(|e| e.kind == "REFERENCES_PACKAGE"
        && e.source == ai
        && e.target
            == EdgeTarget::Node {
                unit: "core/Cargo.toml".into(),
                kind: "Package".into(),
                qualified_name: "package:cargo:workspace:acme:acme-core@0.4.0".into(),
            }));
    let (_, sib) = node(
        a,
        "Package",
        "package:cargo:path:app:../../sibling:sibling@*",
    );
    assert_eq!(sib["external_path"], json!("../sibling"));
    let (_, tok) = node(
        a,
        "Package",
        "package:cargo:git:https://github.com/tokio-rs/tokio:tokio@*",
    );
    assert_eq!(tok["git_reference"], json!("branch:main"));
    let (_, libc) = node(a, "Package", "package:cargo:registry:crates.io:libc@0.2");
    assert_eq!(
        (libc["target"].clone(), libc["dependency_scope"].clone()),
        (json!("cfg(unix)"), json!("dev-dependencies"))
    );
    // The root manifest defines no package: nothing but its file.
    assert_eq!(out["Cargo.toml"].nodes.len(), 1);
    // One unreadable manifest fails the group (the retained extraction is all or nothing).
    let p = TreeSitterParser::new();
    assert!(
        p.extract_group(
            "cargo",
            "acme",
            &[("Cargo.toml".into(), b"[package\n".to_vec())],
            &|| false
        )
        .is_err()
    );
}

#[test]
fn swift_packages_targets_pins_and_xcode_modules_are_identified() {
    let manifest = r#"// swift-tools-version:5.9
import PackageDescription
let package = Package(
    name: "Acme", // the package
    dependencies: [
        .package(url: "https://github.com/apple/swift-log.git", from: "1.5.0"),
        .package(path: "../Local"),
        .package(id: "acme.tools", exact: "2.0.0"),
    ],
    targets: [
        .target(name: "AcmeKit", dependencies: [.product(name: "Logging", package: "swift-log")]),
        .testTarget(name: "AcmeKitTests", dependencies: [.target(name: "AcmeKit")]),
    ]
)
"#;
    let resolved = r#"{ "pins": [ { "identity": "swift-log", "kind": "remoteSourceControl",
        "location": "https://github.com/apple/swift-log.git", "state": { "revision": "abc", "version": "1.5.4" } } ], "version": 2 }"#;
    let pbx = r#"// !$*UTF8*$!
{ objects = {
    P1 = { isa = PBXProject; buildConfigurationList = L1; };
    L1 = { isa = XCConfigurationList; buildConfigurations = ( C1, C2, ); };
    C1 = { isa = XCBuildConfiguration; name = Debug; buildSettings = { PRODUCT_NAME = "$(TARGET_NAME)"; }; };
    C2 = { isa = XCBuildConfiguration; name = Release; buildSettings = { }; };
    T1 = { isa = PBXNativeTarget; name = "My App"; productType = "com.apple.product-type.application"; };
}; rootObject = P1; }"#;
    let out = group(
        "swiftpm",
        "acme",
        &[
            ("Package.swift", manifest),
            ("Package.resolved", resolved),
            ("App/App.xcodeproj/project.pbxproj", pbx),
        ],
    );
    let m = &out["Package.swift"];
    let (di, _) = node(m, "Package", "package:swiftpm:local:acme:acme@*");
    let (li, log) = node(
        m,
        "Package",
        "package:swiftpm:scm:github.com/apple/swift-log@upToNextMajor:1.5.0",
    );
    assert_eq!(log["swift_products"], json!("Logging"));
    assert!(m.edges.iter().any(|e| e.kind == "REFERENCES_PACKAGE"
        && e.source == di
        && e.target == EdgeTarget::Local(li)));
    let (_, local) = node(m, "Package", "package:swiftpm:path:acme:../local@*");
    assert_eq!(local["external_path"], json!("../Local"));
    node(
        m,
        "Package",
        "package:swiftpm:registry:acme.tools@exact:2.0.0",
    );
    let (_, kit) = node(m, "Module", "acme:Package.swift#target:AcmeKit");
    assert_eq!(kit["source_path"], json!("Sources/AcmeKit"));
    let (_, tests) = node(m, "Module", "acme:Package.swift#target:AcmeKitTests");
    assert_eq!(tests["source_path"], json!("Tests/AcmeKitTests"));
    // The pin shares the declared dependency's key.
    let (_, pin) = node(
        &out["Package.resolved"],
        "Package",
        "package:swiftpm:scm:github.com/apple/swift-log@1.5.4",
    );
    assert_eq!(pin["dependency_kind"], json!("pinned"));
    assert_eq!(pin["package_key"], log["package_key"]);
    let (_, app) = node(
        &out["App/App.xcodeproj/project.pbxproj"],
        "Module",
        "acme:App/App.xcodeproj/project.pbxproj#target:My App",
    );
    assert_eq!(app["module_name"], json!("My_App"));
    assert_eq!(app["project_directory"], json!("App"));
}

#[test]
fn csproj_files_name_their_project_and_nuget_references() {
    let csproj = r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup>
        <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
        <PackageReference Include="Serilog" Version="3.1.1" />
    </ItemGroup></Project>"#;
    let out = group("dotnet", "svc", &[("src/Api/Api.csproj", csproj)]);
    let x = &out["src/Api/Api.csproj"];
    let (pi, _) = node(x, "DotnetProject", "svc:project:Api");
    let (ni, n) = node(x, "NuGetPackage", "nuget:Newtonsoft.Json");
    assert_eq!(n["version"], json!("13.0.3"));
    assert!(x.edges.iter().any(|e| e.kind == "REFERENCES_PACKAGE"
        && e.source == pi
        && e.target == EdgeTarget::Local(ni)));
    let m = extract("go.mod", "module example.com/svc\n\ngo 1.22\n");
    assert!(
        m.nodes
            .iter()
            .any(|n| n.kind == "GoModule" && n.properties["module"] == json!("example.com/svc"))
    );
}

#[test]
fn unicode_import_aliases_do_not_abort_endpoint_extraction() {
    let x = extract(
        "src/lib.rs",
        "use crate::pà as x;\nuse axum::{Router, routing::get};\nfn app() -> Router { Router::new().route(\"/ok\", get(x::handler)) }\n",
    );
    assert!(routes(&x).iter().any(|(name, _)| name == "GET /ok"));
    assert_eq!(handles(&x, "GET /ok").unwrap().0, "handler");
}
