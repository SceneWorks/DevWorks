//! Signed .NET bundle qualification through the production package host and graph service.
use super::semantic_fixture::{
    configure, git, index, invocations, overlay, overlay_layer, published,
};
use super::*;
use devworks_core::ports::packages::PackageInspector;
use devworks_core::ports::packages::dotnet::{PACKAGE, SDK_RUNTIME};
use std::os::unix::fs::symlink;

const CS_PROJECT: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
</Project>"#;
const VB_PROJECT: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><TargetFramework>net10.0</TargetFramework><RootNamespace>Client</RootNamespace></PropertyGroup>
  <ItemGroup><ProjectReference Include="../shared/Shared.csproj"/></ItemGroup>
</Project>"#;

#[test]
#[ignore = "native macOS arm64 qualification requires the supported .NET SDK and a signed bundle"]
fn native_dotnet_pack_indexes_csharp_and_visual_basic() {
    let _serial = serial();
    let bundle = std::env::var_os("DEVWORKS_DOTNET_PACK_BUNDLE").expect("signed .NET bundle");
    let dotnet = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|path| path.join("dotnet"))
        .find(|path| path.is_file())
        .expect("dotnet installation")
        .canonicalize()
        .unwrap();
    let mut app = App::unbooted(Harness::new(), None, None);
    app.indexing = true;
    let probe = app.h.root.join("dotnet-probe");
    std::fs::create_dir(&probe).unwrap();
    let selected = probe.join("dotnet");
    symlink(&dotnet, &selected).unwrap();
    app.probe_path = Some(probe.to_str().unwrap().into());
    app.boot();
    assert!(
        matches!(app.runtimes.observations().get(SDK_RUNTIME), Some(RuntimeObservation::Found(s)) if s.starts_with("10.0.2")),
        "{:?}",
        app.runtimes.observations()
    );
    configure(&app, PACKAGE);
    let bytes = std::fs::read(bundle).unwrap();
    let inspected = BundleInspector
        .inspect(&bytes)
        .expect("signed package inspection");
    assert_eq!(inspected.targets.len(), 1);
    assert_eq!(inspected.targets[0].target_triple, host_target_triple());
    let artifact = app.save(&app.user, &bytes);
    package(app.lifecycle(
        &app.user,
        "addons.install",
        json!({"kind":"Install","bundle":artifact,"expectedRegistryRevision":app.registry().to_string()}),
    ));
    app.until(PACKAGE, ".NET SDK is available", |record| {
        availability(record) == "Ready"
    });
    app.ready(PACKAGE);

    let root = app.sources.join("mixed-dotnet");
    for (path, content) in [
        ("shared/Shared.csproj", CS_PROJECT),
        (
            "shared/Values.cs",
            "namespace Fixture; public class Values { public static int Answer(int a) => a; public static string Answer(string a) => a; }\n",
        ),
        (
            "shared/Extra.cs",
            "namespace Fixture; public class Extra {}\n",
        ),
        ("client/Client.vbproj", VB_PROJECT),
        (
            "client/Entry.vb",
            "' café 🚀\r\nPublic Class Entry\r\nPublic Function Run() As Integer\r\nReturn Fixture.Values.Answer(42)\r\nEnd Function\r\nEnd Class\r\n",
        ),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "mixed language application"]);

    let discover = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"dotnet-discover",
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
        panic!("discovery did not return the fixture repository");
    };
    let repository = value.items.0[0].id.clone();
    let project = ProjectId(id("project-a"));
    let member = repository.clone();
    let membership = project.clone();
    write(&*app.store, move |tx| {
        tx.projects().set_members(&membership, &[member])
    })
    .unwrap();
    index(&app, &repository, "indexing.index");
    let initial = published(&app, &repository, None);
    assert_eq!(initial.manifest["packs"][0]["id"], PACKAGE, "{initial:?}");
    let nodes = |generation: &Id, path: &str| {
        read(&*app.store, |snapshot| {
            snapshot.indexing().nodes(generation, path)
        })
    };
    let edges = |generation: &Id, path: &str| {
        read(&*app.store, |snapshot| {
            snapshot.indexing().edges(generation, path)
        })
    };
    let answer = nodes(&initial.id, "shared/Values.cs")
        .into_iter()
        .find(|node| {
            node.payload["properties"]["documentationId"] == "M:Fixture.Values.Answer(System.Int32)"
        })
        .expect("Roslyn C# overload");
    assert!(
        nodes(&initial.id, "client/Client.vbproj")
            .iter()
            .any(|node| node.kind == "DotnetProject")
    );
    assert!(
        edges(&initial.id, "client/Entry.vb").iter().any(|edge| {
            edge.kind == "CALLS"
                && edge.payload["resolution"] == "Resolved"
                && edge.payload["provenance"]["extractor"] == PACKAGE
                && edge.target_node_id == answer.node_id
        }),
        "{:?}",
        edges(&initial.id, "client/Entry.vb")
    );

    let graph_frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,
        "queryId":app.key("dotnet-graph"),"scope":{"kind":"Project","projectId":"project-a"},
        "view":{"kind":"Published","repositoryIds":[repository]},
        "query":{"kind":"Path","selection":{"target":{"kind":"Node","nodeId":answer.node_id},
            "direction":"Inbound","depth":3}},
        "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let agent = app.agent.clone().with_lineage(Some(project), None, None);
    let query = |principal: &InvocationContext| {
        let reply = app
            .app()
            .query(
                principal,
                "code.call_path",
                graph_frame.to_string().as_bytes(),
            )
            .unwrap();
        let DomainQueryReply::Result { result, .. } = reply else {
            panic!("{reply:?}");
        };
        serde_json::to_value(result).unwrap()
    };
    let ui = query(&app.user);
    assert!(ui.to_string().contains("Run"), "{ui}");
    assert_eq!(ui, query(&agent));

    let count = invocations(&app, PACKAGE);
    assert_eq!(count, 1);
    index(&app, &repository, "indexing.index");
    let reused = published(&app, &repository, Some(&initial.id));
    assert_eq!(invocations(&app, PACKAGE), count);

    let worktree = read(&*app.store, |snapshot| {
        Ok(snapshot.indexing().worktrees(&repository)?[0]
            .worktree_id
            .clone())
    });
    std::fs::write(
        root.join("client/Entry.vb"),
        "Public Class Entry\nEnd Class\n",
    )
    .unwrap();
    let dirty = overlay_layer(&app, &repository, "worktreeId", json!(worktree));
    assert!(
        !edges(&dirty.id, "client/Entry.vb")
            .iter()
            .any(|edge| edge.kind == "CALLS")
    );
    assert_eq!(published(&app, &repository, None).id, reused.id);

    std::fs::rename(
        root.join("shared/Values.cs"),
        root.join("shared/Renamed.cs"),
    )
    .unwrap();
    std::fs::remove_file(root.join("shared/Extra.cs")).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "rename and delete"]);
    git(&root, &["branch", "mixed-review"]);
    let branch = overlay(&app, &repository, "refs/heads/mixed-review");
    let delta = read(&*app.store, |snapshot| {
        snapshot.indexing().units(&branch.id, None, 500)
    });
    assert!(
        delta
            .iter()
            .any(|unit| unit.unit_key == "shared/Values.cs" && unit.action == "Delete")
    );
    assert!(
        delta
            .iter()
            .any(|unit| unit.unit_key == "shared/Extra.cs" && unit.action == "Delete")
    );
    assert!(
        nodes(&branch.id, "shared/Renamed.cs")
            .iter()
            .any(|node| node.payload["properties"]["documentationId"]
                == "M:Fixture.Values.Answer(System.Int32)")
    );

    // Build targets in captured XML are data. A build would create this marker in the fixture.
    let unsupported = CS_PROJECT.replace("</Project>",
        r#"<Target Name="Unexpected" BeforeTargets="BeforeBuild"><WriteLinesToFile File="canary" Lines="ran"/></Target></Project>"#);
    std::fs::write(root.join("shared/Shared.csproj"), unsupported).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "unsupported repository target"]);
    let failed_run = index(&app, &repository, "indexing.index");
    let failed = published(&app, &repository, Some(&reused.id));
    assert_eq!(failed.manifest["packs"], json!([]), "{failed:?}");
    assert_eq!(failed.manifest["coverage"]["state"], "Partial");
    assert!(!root.join("shared/canary").exists());
    let recorded = read(&*app.store, |snapshot| snapshot.indexing().run(&failed_run)).unwrap();
    assert!(
        recorded
            .warnings
            .iter()
            .any(|warning| warning.message.contains("dotnet-project-unsupported"))
    );
    assert!(
        nodes(&failed.id, "shared/Renamed.cs")
            .iter()
            .any(|node| node.name == "Values")
    );

    let host = app.app().packages().unwrap();
    std::fs::remove_file(&selected).unwrap();
    host.refresh_runtimes();
    let count = invocations(&app, PACKAGE);
    assert!(matches!(
        app.runtimes.observations().get(SDK_RUNTIME),
        Some(RuntimeObservation::Missing(_))
    ));
    index(&app, &repository, "indexing.reindex");
    let missing = published(&app, &repository, Some(&failed.id));
    assert_eq!(missing.manifest["packs"], json!([]));
    assert_eq!(count, invocations(&app, PACKAGE));

    symlink(dotnet, selected).unwrap();
    host.refresh_runtimes();
    app.ready(PACKAGE);
    std::fs::write(root.join("shared/Shared.csproj"), CS_PROJECT).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "restore supported project"]);
    index(&app, &repository, "indexing.reindex");
    let recovered = published(&app, &repository, Some(&missing.id));
    assert_eq!(
        recovered.manifest["packs"][0]["id"], PACKAGE,
        "{recovered:?}"
    );
}
