use super::*;
use devworks_core::providers::context::{
    ContentBlock, GenerationParameters, Message, MessageRole, ProviderContext,
};
use devworks_extension_protocol::package::HookPhase;

#[test]
fn the_current_package_supplies_all_three_exact_content_kinds() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("content-pack", "1.0.0");
    spec.contributions = vec!["skill", "role", "prompt"];
    spec.content = true;
    let digest = digest_of(&package(a.install(&a.user, &spec)));
    for (id, kind) in [("skill", "Skill"), ("role", "Role"), ("prompt", "Prompt")] {
        let raw = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":9,
            "requestKey":a.key("content"),"body":{"kind":"addons.read_content",
            "packageId":"content-pack","contributionId":format!("content-pack/{id}"),"manifestDigest":digest}});
        let result = applied(
            a.app()
                .command(&a.user, "addons.read_content", raw.to_string().as_bytes())
                .unwrap(),
        );
        let OperationResult::AddonsReadContent { value } = result else {
            panic!("{result:?}")
        };
        assert_eq!(value.content_kind.0, kind);
        assert_eq!(value.manifest_digest.as_str(), digest);
        let bytes = a
            .app()
            .artifacts()
            .read_exact(&*a.store, &a.user, &value.artifact, 1024)
            .unwrap()
            .0;
        assert_eq!(bytes, b"Reusable package content.\n");
    }
}

#[test]
fn only_source_bound_context_suggestions_pass_for_the_selected_phase() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("context-pack", "1.0.0");
    spec.contributions = vec![
        "context-assemble",
        "context-decision",
        "context-compact",
        "context-stale",
    ];
    spec.context_hooks = true;
    package(a.install(&a.user, &spec));
    a.ready("context-pack");
    let context = ProviderContext {
        version: 1,
        instructions: vec![],
        instruction_stamps: vec![],
        skill_stamp: None,
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "Current directions".into(),
                cache: None,
            }],
        }],
        parameters: GenerationParameters::default(),
    };
    let host = a.app().packages().unwrap();
    for (phase, name) in [
        (HookPhase::Assemble, "Assemble"),
        (HookPhase::Compact, "Compact"),
        (HookPhase::Decision, "Decision"),
    ] {
        let notes = host.context_suggestions(&a.user, phase, &context);
        assert_eq!(
            notes.len(),
            1,
            "{name}: {notes:?}; logs: {:?}",
            a.logs("context-pack")
        );
        assert!(
            notes[0].ends_with(&format!("Notes for {name}")),
            "{notes:?}"
        );
    }
}

#[test]
fn native_and_scip_packages_publish_semantic_rows_in_a_real_index_generation() {
    let _s = serial();
    let mut a = App::unbooted(Harness::new(), None, None);
    a.indexing = true;
    a.boot();
    let repository = a.sources.join("semantic-repository");
    std::fs::create_dir(&repository).unwrap();
    std::fs::write(repository.join("fixture.rs"), "fn a() {}\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "fixture.rs"],
        vec!["commit", "-q", "-m", "fixture"],
    ] {
        let git = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .current_dir(&repository)
            .output()
            .unwrap();
        assert!(git.status.success(), "{git:?}");
    }
    for (name, format, contribution) in [
        ("pack-native", "Native", "semantic-native"),
        ("pack-scip", "Scip", "semantic-scip"),
        ("pack-stale", "Native", "semantic-stale"),
    ] {
        let mut spec = Spec::new(name, "1.0.0");
        spec.contributions = vec![contribution];
        spec.semantic_format = Some(format);
        package(a.install(&a.user, &spec));
        a.ready(name);
    }
    let base = descriptor("repositories.baseDirectory").unwrap();
    let assignment = serde_json::from_value(json!({"key":"repositories.baseDirectory","value":{
        "schema":base.schema_ref(),"value":a.sources.to_string_lossy(),
    }}))
    .unwrap();
    let user = a.user.clone();
    write(&*a.store, move |tx| {
        devworks_core::settings::service::apply_human_assignments(
            tx,
            &user,
            &ConfigScope::Workspace,
            &[assignment],
            json!({}),
        )
        .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))
    })
    .unwrap();
    let query = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"semantic-discover",
        "body":{"kind":"indexing.discover","scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}});
    let DomainQueryReply::Result {
        result: OperationResult::IndexingDiscover { value },
        ..
    } = a
        .app()
        .query(&a.user, "indexing.discover", query.to_string().as_bytes())
        .unwrap()
    else {
        panic!("repository discovery failed")
    };
    let repo = value
        .items
        .0
        .first()
        .expect("fixture repository")
        .id
        .clone();
    let execute = |operation: &str, mut body: Value| {
        body["kind"] = json!(operation);
        let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":a.key("index"),"body":body});
        applied(
            a.app()
                .command(&a.user, operation, frame.to_string().as_bytes())
                .unwrap(),
        )
    };
    let revision = || {
        read(&*a.store, |s| {
            Ok(s.indexing()
                .source_config(WORKSPACE_SCOPE_KEY)?
                .map_or(1, |config| config.revision))
        })
        .to_string()
    };
    execute(
        "indexing.configure_sources",
        json!({"scope":{"kind":"Workspace"},"expectedRevision":revision(),
        "config":{"structuralLanguages":[],"semanticPackageIds":["pack-native","pack-scip","pack-stale"],
            "excludePatterns":[],"maxFileBytes":"524288"}}),
    );
    execute(
        "indexing.index",
        json!({"scope":{"kind":"Workspace"},"repositoryIds":[repo],
        "expectedSourceRevision":revision(),"views":{"kind":"Published","repositoryIds":[repo]}}),
    );
    let until = Instant::now() + PATIENCE;
    let (nodes, generation) = loop {
        let repo = repo.clone();
        let published = read(&*a.store, move |snapshot| {
            let indexing = snapshot.indexing();
            let Some(pointer) = indexing.published(&repo)? else {
                return Ok(None);
            };
            let view = indexing.view(&pointer.view_id)?.unwrap();
            Ok(Some((
                indexing.nodes(&view.baseline_id, "fixture.rs")?,
                indexing.generation(&view.baseline_id)?.unwrap(),
            )))
        });
        if let Some(published) = published {
            break published;
        }
        assert!(Instant::now() < until, "index was not published");
        super::event_intents::drive(&a, Duration::from_millis(25));
    };
    assert!(
        nodes.iter().any(|node| node.name == "semantic-a"),
        "{nodes:?} {generation:?}"
    );
    assert!(
        nodes.iter().any(|node| node.name == "local 0"),
        "{nodes:?} {generation:?}"
    );
    assert_eq!(generation.manifest["packs"].as_array().unwrap().len(), 2);
    assert_eq!(generation.manifest["coverage"]["state"], "Partial");
    package(a.simple(&a.user, "addons.disable", "Disable", "pack-native"));
    let status = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"stale-semantic-status",
        "schemaRevision":2,"body":{"kind":"indexing.status","repositoryId":repo}});
    let DomainQueryReply::Result {
        result: OperationResult::IndexingStatus { value },
        ..
    } = a
        .app()
        .query(&a.user, "indexing.status", status.to_string().as_bytes())
        .unwrap()
    else {
        panic!("the indexed source should still be visible")
    };
    assert!(
        value
            .freshness
            .expect("freshness")
            .reasons
            .0
            .iter()
            .any(|label| label.0 == "semantic-package-changed")
    );
}
