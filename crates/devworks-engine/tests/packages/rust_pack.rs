//! The real, separately built Rust pack through install, managed process launch, indexing and
//! immutable graph generations on the qualified native target.
use super::*;
use devworks_core::ports::storage::StoredGeneration;
use devworks_core::ports::storage::WORKSPACE_SCOPE_KEY;

fn git(root: &std::path::Path, args: &[&str]) {
    let outcome = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=DW-27 fixture",
            "-c",
            "user.email=fixture@devworks.test",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        outcome.status.success(),
        "{}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

fn published(app: &App, repository: &Id, previous: Option<&Id>) -> StoredGeneration {
    let until = Instant::now() + PATIENCE * 4;
    loop {
        if let Some(current) = read(&*app.store, |snapshot| {
            let indexing = snapshot.indexing();
            let Some(pointer) = indexing.published(repository)? else {
                return Ok(None);
            };
            let view = indexing.view(&pointer.view_id)?.expect("published view");
            indexing.generation(&view.baseline_id)
        }) && previous != Some(&current.id)
        {
            return current;
        }
        assert!(Instant::now() < until, "the repository did not publish");
        super::event_intents::drive(app, Duration::from_millis(30));
    }
}

fn invocations(app: &App) -> usize {
    read(&*app.store, |snapshot| {
        snapshot.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: "devworks-rust-semantic".into(),
            },
            0,
            500,
        )
    })
    .events
    .iter()
    .filter(|event| event.kind == "PackageInvoked")
    .count()
}

fn source_revision(app: &App) -> String {
    read(&*app.store, |snapshot| {
        Ok(snapshot
            .indexing()
            .source_config(WORKSPACE_SCOPE_KEY)?
            .map_or(1, |config| config.revision))
    })
    .to_string()
}

fn overlay(app: &App, repository: &Id) -> StoredGeneration {
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":2,
        "requestKey":app.key("rust-overlay"),"command":{"kind":"RefreshOverlay",
        "repositoryId":repository,"ref":"refs/heads/rust-review",
        "expectedSourceRevision":source_revision(app)}});
    let OperationResult::IndexingRefreshOverlay { value } = applied(
        app.app()
            .command(
                &app.user,
                "indexing.refresh_overlay",
                frame.to_string().as_bytes(),
            )
            .unwrap(),
    ) else {
        panic!("expected an overlay run")
    };
    let until = Instant::now() + PATIENCE;
    loop {
        let run =
            read(&*app.store, |snapshot| snapshot.indexing().run(&value.id)).expect("overlay run");
        if let Some(view) = run.output_view_id {
            return read(&*app.store, |snapshot| {
                let index = snapshot.indexing();
                let view = index.view(&view)?.expect("overlay view");
                index.generation(&view.branch_id.expect("branch generation"))
            })
            .expect("semantic branch generation");
        }
        assert!(Instant::now() < until, "{run:?}");
        super::event_intents::drive(app, Duration::from_millis(30));
    }
}

fn index(app: &App, repository: &Id) {
    let revision = source_revision(app);
    let frame = json!({
        "protocol":"1.0-draft.1","mode":"Command","requestKey":app.key("rust-pack-index"),
        "body":{"kind":"indexing.index","scope":{"kind":"Workspace"},
            "repositoryIds":[repository],"expectedSourceRevision":revision,
            "views":{"kind":"Published","repositoryIds":[repository]}}
    });
    applied(
        app.app()
            .command(&app.user, "indexing.index", frame.to_string().as_bytes())
            .unwrap(),
    );
}

fn configure(app: &App) {
    let descriptor =
        devworks_core::settings::catalog::descriptor("repositories.baseDirectory").unwrap();
    let value = serde_json::from_value(json!({
        "key":"repositories.baseDirectory",
        "value":{"schema":descriptor.schema_ref(),"value":app.sources.to_string_lossy()}
    }))
    .unwrap();
    let user = app.user.clone();
    write(&*app.store, move |tx| {
        devworks_core::settings::service::apply_human_assignments(
            tx,
            &user,
            &ConfigScope::Workspace,
            &[value],
            json!({}),
        )
        .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))
    })
    .unwrap();
    let frame = json!({
        "protocol":"1.0-draft.1","mode":"Command","requestKey":app.key("configure"),
        "body":{"kind":"indexing.configure_sources","scope":{"kind":"Workspace"},"expectedRevision":"1",
            "config":{"structuralLanguages":[],"semanticPackageIds":["devworks-rust-semantic"],
                "excludePatterns":[],"maxFileBytes":"524288"}}
    });
    applied(
        app.app()
            .command(
                &app.user,
                "indexing.configure_sources",
                frame.to_string().as_bytes(),
            )
            .unwrap(),
    );
}

#[test]
#[ignore = "native macOS arm64 qualification requires the pinned rust-analyzer and signed release helper"]
fn native_rust_pack_indexes_a_cargo_workspace_and_fences_changed_deleted_and_failed_sources() {
    let _serial = serial();
    let bundle = std::env::var_os("DEVWORKS_RUST_PACK_BUNDLE").expect("signed native pack bundle");
    let bytes = std::fs::read(bundle).unwrap();
    let mut app = App::unbooted(Harness::new(), None, None);
    app.indexing = true;
    app.probe_path = Some(std::env::var("PATH").expect("qualified toolchain PATH"));
    app.boot();
    configure(&app);
    let artifact = app.save(&app.user, &bytes);
    package(app.lifecycle(
        &app.user,
        "addons.install",
        json!({
            "kind":"Install","bundle":artifact,"expectedRegistryRevision":app.registry().to_string()
        }),
    ));
    app.until(
        "devworks-rust-semantic",
        "the pinned Rust runtimes are available",
        |record| availability(record) == "Ready",
    );
    app.ready("devworks-rust-semantic");

    let root = app.sources.join("rust-workspace");
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::create_dir_all(root.join("shared/src")).unwrap();
    for (path, content) in [
        (
            "Cargo.toml",
            "[workspace]\nresolver='3'\nmembers=['app','shared']\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='fixture-app'\nversion='1.2.3'\nedition='2024'\n[dependencies]\nfixture-shared={path='../shared'}\n",
        ),
        (
            "shared/Cargo.toml",
            "[package]\nname='fixture-shared'\nversion='4.5.6'\nedition='2024'\n",
        ),
        (
            "app/src/lib.rs",
            "pub fn caller() -> u32 { fixture_shared::stable_value() }\n",
        ),
        (
            "app/build.rs",
            "fn main() { panic!(\"the semantic helper executed the repository build script\"); }\n",
        ),
        (
            "shared/src/lib.rs",
            "pub fn stable_value() -> u32 { 3 }\nmod removed;\n",
        ),
        ("shared/src/removed.rs", "pub fn obsolete() {}\n"),
    ] {
        std::fs::write(root.join(path), content).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "original"]);

    let discover = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"rust-pack-discover",
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
        panic!("Rust workspace discovery failed");
    };
    let repository = value
        .items
        .0
        .first()
        .expect("discovered repository")
        .id
        .clone();

    index(&app, &repository);
    let initial = published(&app, &repository, None);
    assert_eq!(
        initial.manifest["packs"][0]["id"], "devworks-rust-semantic",
        "{initial:?}"
    );
    let initial_edges = read(&*app.store, |snapshot| {
        snapshot.indexing().edges(&initial.id, "app/src/lib.rs")
    });
    let target_nodes = read(&*app.store, |snapshot| {
        snapshot.indexing().nodes(&initial.id, "shared/src/lib.rs")
    });
    let target = target_nodes
        .iter()
        .find(|node| {
            node.payload["qualifiedName"].as_str().is_some_and(|name| {
                name.contains("fixture-shared")
                    && name.contains("4.5.6")
                    && name.contains("stable_value")
            })
        })
        .unwrap_or_else(|| panic!("{target_nodes:?}"));
    assert!(
        initial_edges.iter().any(|edge| {
            edge.kind == "REFERENCES"
                && edge.payload["resolution"] == "Resolved"
                && edge.target_node_id == target.node_id
        }),
        "{initial_edges:?}"
    );
    let compiled = invocations(&app);
    assert_eq!(compiled, 1, "one compiler invocation per captured layer");

    index(&app, &repository);
    let reused = published(&app, &repository, Some(&initial.id));
    assert_eq!(
        reused.manifest["packs"], initial.manifest["packs"],
        "{reused:?}"
    );
    assert_eq!(reused.source_digest, initial.source_digest);
    assert_eq!(
        invocations(&app),
        compiled,
        "a matching complete source reuses its semantic rows"
    );

    std::fs::remove_file(root.join("shared/src/removed.rs")).unwrap();
    std::fs::write(
        root.join("shared/src/lib.rs"),
        "pub fn renamed_value() -> u32 { 4 }\n",
    )
    .unwrap();
    // The app's file changes too; neither the old definition nor its old reference may survive.
    std::fs::write(
        root.join("app/src/lib.rs"),
        "pub fn caller() -> u32 { fixture_shared::renamed_value() }\n",
    )
    .unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "changed and deleted"]);
    git(&root, &["branch", "rust-review"]);
    let branch = overlay(&app, &repository);
    assert_eq!(
        branch.manifest["packs"][0]["id"], "devworks-rust-semantic",
        "{branch:?}"
    );
    let overlay_units = read(&*app.store, |snapshot| {
        snapshot.indexing().units(&branch.id, None, 500)
    });
    assert!(
        overlay_units
            .iter()
            .any(|unit| unit.unit_key == "shared/src/removed.rs" && unit.action == "Delete"),
        "{overlay_units:?}"
    );
    assert_eq!(published(&app, &repository, None).id, reused.id);

    index(&app, &repository);
    let changed = published(&app, &repository, Some(&reused.id));
    assert_eq!(
        changed.manifest["packs"][0]["id"], "devworks-rust-semantic",
        "{changed:?}"
    );
    let removed = read(&*app.store, |snapshot| {
        snapshot.indexing().units(&changed.id, None, 500)
    });
    assert!(
        !removed
            .iter()
            .any(|unit| unit.unit_key == "shared/src/removed.rs"),
        "{removed:?}"
    );
    let nodes = read(&*app.store, |snapshot| {
        snapshot.indexing().nodes(&changed.id, "shared/src/lib.rs")
    });
    assert!(
        nodes.iter().any(|node| node.payload["qualifiedName"]
            .as_str()
            .is_some_and(|name| name.contains("renamed_value"))),
        "{nodes:?}"
    );
    assert!(!nodes.iter().any(|node| {
        node.payload["qualifiedName"]
            .as_str()
            .is_some_and(|name| name.contains("stable_value"))
    }));

    std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='fixture-app'\nversion='1.2.3'\nedition='2024'\n[dependencies]\nfixture-shared={path='../../outside'}\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "invalid dependency path"]);
    index(&app, &repository);
    let failed_pack = published(&app, &repository, Some(&changed.id));
    assert_eq!(
        failed_pack.manifest["coverage"]["state"], "Partial",
        "{failed_pack:?}"
    );
    assert_eq!(failed_pack.manifest["packs"], json!([]));
    let nodes = read(&*app.store, |snapshot| {
        snapshot
            .indexing()
            .nodes(&failed_pack.id, "shared/src/lib.rs")
    });
    assert!(
        nodes.iter().any(|node| node.name == "renamed_value"),
        "{nodes:?}"
    );
}
