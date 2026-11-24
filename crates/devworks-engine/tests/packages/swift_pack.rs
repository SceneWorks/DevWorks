//! Signed Apple Swift pack qualification through production package and indexing services.
use super::semantic_fixture::{
    configure, git, index, invocations, overlay, overlay_layer, published,
};
use super::*;
use devworks_core::ports::packages::PackageInspector;
use devworks_core::ports::packages::swift::{PACKAGE, SDK_RUNTIME};

fn fixture_app() -> App {
    let bundle = std::env::var_os("DEVWORKS_SWIFT_PACK_BUNDLE").expect("signed Swift bundle");
    let mut app = App::unbooted(Harness::new(), None, None);
    app.indexing = true;
    app.probe_path = Some("/usr/bin".into());
    app.boot();
    assert_eq!(
        app.runtimes.observations().get(SDK_RUNTIME),
        Some(&RuntimeObservation::Found("6.4.2".into())),
        "{:?}",
        app.runtimes.observations()
    );
    configure(&app, PACKAGE);
    let bytes = std::fs::read(bundle).unwrap();
    let verified = BundleInspector.inspect(&bytes).expect("signed pack");
    assert_eq!(verified.inspection.targets.len(), 1);
    assert_eq!(
        verified.inspection.targets[0].target_triple,
        host_target_triple()
    );
    for target_triple in [
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ] {
        let platform = HostPlatform {
            target_triple: target_triple.into(),
            host_version: "0.1.0".into(),
            protocol_major: 1,
            protocol_minor: 0,
        };
        assert!(
            devworks_core::services::packages::policy::static_incompatibility(
                &verified.inspection,
                &platform
            )
            .is_some()
        );
    }
    let artifact = app.save(&app.user, &bytes);
    package(app.lifecycle(
        &app.user, "addons.install",
        json!({"kind":"Install","bundle":artifact,"expectedRegistryRevision":app.registry().to_string()}),
    ));
    app.until(
        PACKAGE,
        "the qualified Swift environment is available",
        |p| availability(p) == "Ready",
    );
    app.ready(PACKAGE);
    app
}

fn fixture_repository(app: &App, dir: &str, files: &[(&str, &str)]) -> (std::path::PathBuf, Id) {
    let root = app.sources.join(dir);
    for (name, source) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["config", "--local", "core.autocrlf", "false"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial Swift fixture"]);
    let discover = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":app.key("swift-discover"),
        "body":{"kind":"indexing.discover","scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}});
    let DomainQueryReply::Result {
        result: OperationResult::IndexingDiscover { value },
        ..
    } = app
        .app()
        .query(
            &app.user,
            "indexing.discover",
            discover.to_string().as_bytes(),
        )
        .unwrap()
    else {
        panic!("discovery did not find the fixture");
    };
    let repository = value.items.0[0].id.clone();
    let member = repository.clone();
    write(&*app.store, move |tx| {
        tx.projects()
            .set_members(&ProjectId(id("project-a")), &[member])
    })
    .unwrap();
    // A project section inherits the default exclusions until it is configured.
    // This fixture admits Sources/Kit/Build to exercise normal Swift files in that directory.
    let config = json!({
        "protocol":"1.0-draft.1","mode":"Command","requestKey":app.key("swift-configure-project"),
        "body":{"kind":"indexing.configure_sources","scope":{"kind":"Project","projectId":"project-a"},
            "expectedRevision":"1","config":{"structuralLanguages":[],"semanticPackageIds":[],
                "excludePatterns":[],"maxFileBytes":"524288"}}
    });
    applied(
        app.app()
            .command(
                &app.user,
                "indexing.configure_sources",
                config.to_string().as_bytes(),
            )
            .unwrap(),
    );
    (root, repository)
}

#[test]
#[ignore = "requires the qualified Xcode installation and signed Swift pack on macOS arm64"]
fn native_swift_pack_indexes_local_modules_and_preserves_independent_targets() {
    let _serial = serial();
    let app = fixture_app();
    let manifest = r#"// swift-tools-version:6.0
import PackageDescription
let package = Package(
    name: "Fixture", platforms: [.macOS(.v14)],
    dependencies: [.package(name: "Local", path: "Packages/Local")],
    targets: [
        .target(name: "Kit", dependencies: [.product(name: "Shared", package: "Local")], exclude: ["Excluded.swift"]),
        .executableTarget(name: "Client", dependencies: ["Kit"]),
        .target(name: "Independent"),
        .target(name: "Mixed")
    ]
)
"#;
    const CLIENT: &str = "import Kit\r\n// café 🚀\r\n#sourceLocation(file: \"other.swift\", line: 500)\r\npublic func run() -> Int { Counter(value: 2).count(3) + packageValue() }\r\n#sourceLocation()\r\n@main struct Tool { static func main() { _ = run() } }\r\n";
    let (root, repository) = fixture_repository(
        &app,
        "swift-capture",
        &[
            ("Package.swift", manifest),
            (
                "Packages/Local/Package.swift",
                r#"// swift-tools-version:6.0
import PackageDescription
let package = Package(name: "Local", platforms: [.macOS(.v14)],
    products: [.library(name: "Shared", targets: ["Shared"])], targets: [.target(name: "Shared")])
"#,
            ),
            (
                "Packages/Local/Sources/Shared/Base.swift",
                "public func seed() -> Int { 3 }\n",
            ),
            (
                "Sources/Kit/Counter.swift",
                r#"import Shared
public protocol Counting { func count(_ step: Int) -> Int }
public struct Counter: Counting {
    public let value: Int
    public init(value: Int) { self.value = value }
    public func count(_ step: Int) -> Int { value + step + seed() }
    public func count(_ step: String) -> Int { value + step.count }
}
extension Counter { public func extensionMember() -> Int { value } }
"#,
            ),
            (
                "Sources/Kit/Build/Access.swift",
                "package func packageValue() -> Int { 3 }\n",
            ),
            ("Sources/Kit/Extra.swift", "public struct Extra {}\n"),
            ("Sources/Mixed/Mixed.swift", "public struct Mixed {}\n"),
            (
                "Sources/Mixed/native.c",
                "int native(void) { return 42; }\n",
            ),
            ("Sources/Kit/Excluded.swift", "public struct Excluded {}\n"),
            ("Sources/Client/Entry.swift", CLIENT),
            (
                "Sources/Independent/Marker.swift",
                "public func available() { Swift.print(1) }; public func print(_ v: Int) {}\n",
            ),
        ],
    );
    let run = index(&app, &repository, "indexing.index");
    let initial = published(&app, &repository, None);
    let details = read(&*app.store, |s| s.indexing().run(&run)).unwrap();
    assert_eq!(initial.manifest["packs"][0]["id"], PACKAGE, "{details:?}");
    let nodes =
        |generation: &Id, path: &str| read(&*app.store, |s| s.indexing().nodes(generation, path));
    let edges =
        |generation: &Id, path: &str| read(&*app.store, |s| s.indexing().edges(generation, path));
    let kit = nodes(&initial.id, "Sources/Kit/Counter.swift");
    let callee = kit
        .iter()
        .find(|node| {
            node.name == "count"
                && node.payload["properties"]["usr"] == "s:3Kit7CounterV5countyS2iF"
        })
        .expect("Swift compiler Int overload");
    let constructor = kit
        .iter()
        .find(|node| {
            node.kind == "Constructor"
                && node.payload["properties"]["semanticBasis"] == "swift-indexstore"
        })
        .expect("Swift compiler constructor");
    let package_access = nodes(&initial.id, "Sources/Kit/Build/Access.swift")
        .into_iter()
        .find(|node| {
            node.name == "packageValue"
                && node.payload["properties"]["semanticBasis"] == "swift-indexstore"
        })
        .expect("package-access function under a Build directory");
    let module = nodes(&initial.id, "Package.swift")
        .into_iter()
        .find(|node| node.name == "Kit" && node.kind == "Module")
        .expect("structural SwiftPM module");
    let incoming = edges(&initial.id, "Sources/Client/Entry.swift");
    for expected in [
        &callee.node_id,
        &constructor.node_id,
        &package_access.node_id,
    ] {
        assert!(
            incoming.iter().any(|edge| edge.kind == "CALLS"
                && &edge.target_node_id == expected
                && edge.payload["resolution"] == "Resolved"
                && edge.payload["provenance"]["extractor"] == PACKAGE),
            "{expected:?}: {incoming:?}"
        );
    }
    assert!(
        incoming.iter().any(|edge| edge.kind == "IMPORTS"
            && edge.target_node_id == module.node_id
            && edge.payload["resolution"] == "Resolved"
            && edge.payload["provenance"]["extractor"] == PACKAGE),
        "{incoming:?}"
    );
    let client = nodes(&initial.id, "Sources/Client/Entry.swift");
    let caller = client
        .iter()
        .find(|node| node.name == "run" && node.payload["properties"]["compilerTokenSpan"] == true)
        .unwrap();
    assert_eq!(
        caller.payload["span"]["startByte"],
        CLIENT.find("run()").unwrap()
    );
    assert!(
        client
            .iter()
            .any(|node| node.name == "main"
                && node.payload["properties"]["compilerTokenSpan"] == true)
    );
    let extension = kit
        .iter()
        .find(|node| {
            node.kind == "Extension" && node.payload["properties"]["compilerTokenSpan"] == true
        })
        .unwrap();
    let member = kit
        .iter()
        .find(|node| {
            node.name == "extensionMember"
                && node.payload["properties"]["compilerTokenSpan"] == true
        })
        .unwrap();
    let relations = edges(&initial.id, "Sources/Kit/Counter.swift");
    assert!(relations.iter().any(|edge| edge.kind == "DEFINES_METHOD"
        && edge.source_node_id == extension.node_id
        && edge.target_node_id == member.node_id));
    for kind in ["IMPLEMENTS", "EXTENDS", "DEFINES_METHOD", "REFERENCES"] {
        assert!(relations.iter().any(|edge| edge.kind == kind
            && edge.payload["provenance"]["extractor"] == PACKAGE), "{kind}: {relations:?}");
    }
    let units = read(&*app.store, |s| s.indexing().units(&initial.id, None, 500));
    for (path, reason) in [
        ("Sources/Kit/Excluded.swift", "swift-source-excluded"),
        ("Sources/Mixed/Mixed.swift", "swift-project-unsupported"),
    ] {
        assert!(
            units.iter().any(|unit| unit.unit_key == path
                && unit.payload["incomplete"]
                    .as_array()
                    .is_some_and(|gaps| gaps.iter().any(|gap| gap == reason))),
            "{path} should report {reason}"
        );
    }
    let foreign = edges(&initial.id, "Sources/Independent/Marker.swift");
    assert!(
        foreign.iter().any(|edge| edge.kind == "CALLS"
            && edge.payload["properties"]["compiler_bound_unpublished"] == true
            && edge.payload["properties"]["displayName"]
                .as_str()
                .is_some_and(|name| name.starts_with("print("))
            && edge.payload["resolution"] == "Unresolved"),
        "{foreign:?}"
    );

    let request = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":app.key("swift-graph"),
        "scope":{"kind":"Project","projectId":"project-a"},"view":{"kind":"Published","repositoryIds":[repository]},
        "query":{"kind":"Path","selection":{"target":{"kind":"Node","nodeId":callee.node_id},"direction":"Inbound","depth":3}},
        "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let query = |principal: &InvocationContext| {
        let reply = app
            .app()
            .query(principal, "code.call_path", request.to_string().as_bytes())
            .unwrap();
        let DomainQueryReply::Result { result, .. } = reply else {
            panic!("{reply:?}");
        };
        serde_json::to_value(result).unwrap()
    };
    let ui = query(&app.user);
    let agent = query(&app.agent.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        None,
        None,
    ));
    assert_eq!(ui["value"]["result"], agent["value"]["result"]);
    assert!(ui.to_string().contains("\"name\":\"run\""));

    let count = invocations(&app, PACKAGE);
    index(&app, &repository, "indexing.index");
    let reused = published(&app, &repository, Some(&initial.id));
    assert_eq!(count, invocations(&app, PACKAGE));
    index(&app, &repository, "indexing.reindex");
    let reused = published(&app, &repository, Some(&reused.id));
    assert!(invocations(&app, PACKAGE) > count);
    assert_eq!(reused.manifest["packs"][0]["id"], PACKAGE);
    let worktree = read(&*app.store, |s| {
        Ok(s.indexing().worktrees(&repository)?[0].worktree_id.clone())
    });
    std::fs::write(
        root.join("Sources/Client/Entry.swift"),
        "import Kit\npublic func run() -> Int { missing }\n",
    )
    .unwrap();
    let dirty = overlay_layer(&app, &repository, "worktreeId", json!(worktree));
    let delta = read(&*app.store, |s| s.indexing().units(&dirty.id, None, 500));
    assert!(
        delta
            .iter()
            .any(|unit| unit.unit_key == "Sources/Client/Entry.swift"
                && unit.payload["incomplete"]
                    .to_string()
                    .contains("swift-compiler-failed")),
        "{dirty:?}"
    );
    assert!(
        dirty.manifest["packs"].to_string().contains(PACKAGE),
        "{dirty:?}"
    );
    assert!(
        !edges(&dirty.id, "Sources/Client/Entry.swift")
            .iter()
            .any(|edge| edge.payload["provenance"]["extractor"] == PACKAGE)
    );
    assert_eq!(published(&app, &repository, None).id, reused.id);

    std::fs::write(root.join("Sources/Client/Entry.swift"), CLIENT).unwrap();
    std::fs::rename(
        root.join("Sources/Kit/Counter.swift"),
        root.join("Sources/Kit/Moved.swift"),
    )
    .unwrap();
    std::fs::remove_file(root.join("Sources/Kit/Extra.swift")).unwrap();
    git(&root, &["add", "-A"]);
    git(
        &root,
        &["commit", "-qm", "rename and delete Swift declarations"],
    );
    git(&root, &["branch", "swift-review"]);
    let branch = overlay(&app, &repository, "refs/heads/swift-review");
    let delta = read(&*app.store, |s| s.indexing().units(&branch.id, None, 500));
    for removed in ["Sources/Kit/Counter.swift", "Sources/Kit/Extra.swift"] {
        assert!(
            delta
                .iter()
                .any(|unit| unit.unit_key == removed && unit.action == "Delete")
        );
    }
    assert!(
        nodes(&branch.id, "Sources/Kit/Moved.swift")
            .iter()
            .any(|node| node.payload["properties"]["usr"] == callee.payload["properties"]["usr"])
    );

    let canary = manifest.replace("let package", r#"let marker = try! "ran".write(toFile: "manifest-canary", atomically: true, encoding: .utf8)
let package"#);
    std::fs::write(root.join("Package.swift"), canary).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "unsupported manifest canary"]);
    let failed = index(&app, &repository, "indexing.index");
    let fallback = published(&app, &repository, Some(&reused.id));
    assert_eq!(fallback.manifest["packs"], json!([]));
    assert!(!root.join("manifest-canary").exists());
    let run = read(&*app.store, |s| s.indexing().run(&failed)).unwrap();
    assert!(
        run.warnings
            .iter()
            .any(|warning| warning.message.contains("swift-project-unsupported"))
    );
    assert!(
        nodes(&fallback.id, "Sources/Kit/Moved.swift")
            .iter()
            .any(|node| node.name == "Counter")
    );

    std::fs::write(root.join("Package.swift"), manifest).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "restore literal package"]);
    let before = invocations(&app, PACKAGE);
    // Simulate a stale discovery receipt without altering the installed Xcode files.
    let mut previous = app.runtimes.swift().unwrap();
    previous.sdk_settings = "f".repeat(64);
    app.runtimes.set_probed_with_swift(
        app.runtimes.observations(),
        Default::default(),
        Some(previous),
    );
    index(&app, &repository, "indexing.reindex");
    let stale = published(&app, &repository, Some(&fallback.id));
    assert_eq!(stale.manifest["packs"], json!([]));
    assert_eq!(before, invocations(&app, PACKAGE));
    assert_eq!(
        app.runtimes.observations().get(SDK_RUNTIME),
        Some(&RuntimeObservation::Missing("swift-runtime-changed".into())),
    );

    let host = app.app().packages().unwrap();
    assert_eq!(
        host.refresh_runtimes()[SDK_RUNTIME],
        RuntimeObservation::Found("6.4.2".into())
    );
    index(&app, &repository, "indexing.index");
    let rebuilt = published(&app, &repository, Some(&stale.id));
    assert_eq!(rebuilt.manifest["packs"][0]["id"], PACKAGE);

    package(app.simple(&app.user, "addons.disable", "Disable", PACKAGE));
    index(&app, &repository, "indexing.index");
    let disabled = published(&app, &repository, Some(&rebuilt.id));
    assert_eq!(disabled.manifest["packs"], json!([]));
    assert!(
        nodes(&disabled.id, "Sources/Kit/Moved.swift")
            .iter()
            .any(|node| node.name == "Counter")
    );
}

#[test]
#[ignore = "requires the qualified Xcode installation and signed Swift pack on macOS arm64"]
fn native_swift_pack_qualifies_xctest_and_apple_macros_with_cold_cache() {
    let _serial = serial();
    let app = fixture_app();
    let (_, repository) = fixture_repository(
        &app,
        "swift-cold",
        &[
            (
                "Package.swift",
                r#"// swift-tools-version:6.0
import PackageDescription
let package = Package(name: "Cold", platforms: [.macOS(.v14)], targets: [.testTarget(name: "NativeTests"), .target(name: "ExternalMacro")])
"#,
            ),
            (
                "Tests/NativeTests/Probe.swift",
                r#"import Foundation
import Observation
import XCTest
import Testing
@Observable final class Counter { var value = 1 }
final class ProbeTests: XCTestCase { func testCount() { XCTAssertEqual(Counter().value, 1) } }
@Test func nativeMacroCase() { let value = 1; #expect(value == 1) }
"#,
            ),
            (
                "Sources/ExternalMacro/Unavailable.swift",
                r#"@freestanding(expression)
public macro outside() -> Int = #externalMacro(module: "RepoMacros", type: "Canary")
public let result = #outside()
"#,
            ),
        ],
    );
    let started = Instant::now();
    let run = index(&app, &repository, "indexing.index");
    let published = published(&app, &repository, None);
    let run = read(&*app.store, |s| s.indexing().run(&run)).unwrap();
    assert_eq!(published.manifest["packs"][0]["id"], PACKAGE, "{run:?}");
    let units = read(&*app.store, |s| {
        s.indexing().units(&published.id, None, 500)
    });
    assert!(
        units.iter().any(
            |unit| unit.unit_key == "Sources/ExternalMacro/Unavailable.swift"
                && unit.payload["incomplete"]
                    .to_string()
                    .contains("swift-compiler-failed")
        ),
        "a repository-defined macro must remain unresolved"
    );
    let nodes = read(&*app.store, |s| {
        s.indexing()
            .nodes(&published.id, "Tests/NativeTests/Probe.swift")
    });
    for name in ["Counter", "testCount", "nativeMacroCase"] {
        assert!(
            nodes
                .iter()
                .any(|node| node.name == name
                    && node.payload["properties"]["compilerTokenSpan"] == true),
            "{name}: {nodes:?}"
        );
    }
    eprintln!(
        "signed cold Swift fixture completed in {:?}",
        started.elapsed()
    );
}

#[test]
#[ignore = "requires the qualified Xcode installation and signed Swift pack on macOS arm64"]
fn native_swift_pack_reports_input_and_output_bounds_with_structural_fallback() {
    let _serial = serial();
    let app = fixture_app();
    let long_names: Vec<_> = (0..10)
        .map(|file| {
            let members = (0..375)
                .map(|index| format!("    public func value_{index}() -> Int {{ 0 }}\n"))
                .collect::<String>();
            format!(
                "public struct Type_{file}_{} {{\n{members}}}\n",
                "a".repeat(1400)
            )
        })
        .collect();
    let paths: Vec<_> = (0..long_names.len())
        .map(|i| format!("Sources/Big/Part{i}.swift"))
        .collect();
    let mut files = vec![(
        "Package.swift",
        "// swift-tools-version:6.0\nimport PackageDescription\nlet package = Package(name: \"Big\", targets: [.target(name: \"Big\")])\n",
    )];
    files.extend(
        paths
            .iter()
            .zip(&long_names)
            .map(|(path, source)| (path.as_str(), source.as_str())),
    );
    let (root, repository) = fixture_repository(&app, "swift-bounds", &files);
    let run = index(&app, &repository, "indexing.index");
    let limited = published(&app, &repository, None);
    let inspect = |run: &Id| {
        read(&*app.store, |s| s.indexing().run(run))
            .unwrap()
            .warnings
    };
    let warnings = inspect(&run);
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("swift-output-budget")),
        "{warnings:?}"
    );
    assert_eq!(limited.manifest["packs"], json!([]));
    let nodes = read(&*app.store, |s| s.indexing().nodes(&limited.id, &paths[0]));
    assert!(
        nodes.iter().any(|node| node.name == "value_0"
            && node.payload["properties"]["compilerTokenSpan"] != true)
    );

    // Each file stays within its individual cap. Collectively they exceed the batch budget.
    let padding = format!("// {}\n", "a".repeat(400_000));
    for part in 0..12 {
        std::fs::write(
            root.join(format!("Sources/Big/Padding{part}.swift")),
            &padding,
        )
        .unwrap();
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "exceed aggregate Swift input"]);
    let before = invocations(&app, PACKAGE);
    let run = index(&app, &repository, "indexing.reindex");
    let fallback = published(&app, &repository, Some(&limited.id));
    let warnings = inspect(&run);
    assert!(
        warnings.iter().any(|w| w
            .message
            .contains("repository source exceeds the semantic input budget")),
        "{warnings:?}"
    );
    assert_eq!(fallback.manifest["packs"], json!([]));
    assert_eq!(invocations(&app, PACKAGE), before);
}
