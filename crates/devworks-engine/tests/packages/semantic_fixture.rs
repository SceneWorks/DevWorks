use super::*;
use devworks_core::ports::storage::StoredGeneration;
use devworks_core::ports::storage::WORKSPACE_SCOPE_KEY;

pub(super) fn git(root: &std::path::Path, args: &[&str]) {
    let outcome = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=DevWorks semantic fixture",
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

pub(super) fn published(app: &App, repository: &Id, previous: Option<&Id>) -> StoredGeneration {
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

pub(super) fn invocations(app: &App, package: &str) -> usize {
    read(&*app.store, |snapshot| {
        snapshot.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: package.into(),
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

pub(super) fn overlay(app: &App, repository: &Id, reference: &str) -> StoredGeneration {
    overlay_layer(app, repository, "ref", json!(reference))
}

pub(super) fn overlay_layer(
    app: &App,
    repository: &Id,
    selector: &str,
    target: Value,
) -> StoredGeneration {
    let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":2,
        "requestKey":app.key("semantic-overlay"),"command":{"kind":"RefreshOverlay",
        "repositoryId":repository,"expectedSourceRevision":source_revision(app)}});
    frame["command"][selector] = target;
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
                let layer = if selector == "worktreeId" {
                    view.worktree_id
                } else {
                    view.branch_id
                };
                index.generation(&layer.expect("requested overlay generation"))
            })
            .expect("semantic branch generation");
        }
        assert!(Instant::now() < until, "{run:?}");
        super::event_intents::drive(app, Duration::from_millis(30));
    }
}

pub(super) fn index(app: &App, repository: &Id, operation: &str) {
    let revision = source_revision(app);
    let frame = json!({
        "protocol":"1.0-draft.1","mode":"Command","requestKey":app.key("semantic-pack-index"),
        "body":{"kind":operation,"scope":{"kind":"Workspace"},
            "repositoryIds":[repository],"expectedSourceRevision":revision,
            "views":{"kind":"Published","repositoryIds":[repository]}}
    });
    applied(
        app.app()
            .command(&app.user, operation, frame.to_string().as_bytes())
            .unwrap(),
    );
}

pub(super) fn configure(app: &App, package: &str) {
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
            "config":{"structuralLanguages":[],"semanticPackageIds":[package],
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
