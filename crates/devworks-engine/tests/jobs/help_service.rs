//! Real Help gateway, immutable SQLite catalog and managed artifacts; no inference or network.
use super::*;
use devworks_core::authority::service::bind_principal;
use devworks_core::protocol::results::DomainQueryReply;
use devworks_core::settings::catalog::AgentRole;

fn query(
    app: &HelpApp,
    ctx: &InvocationContext,
    operation: &str,
    mut body: Value,
    revision: u32,
) -> DomainQueryReply {
    if body.get("cursor") == Some(&Value::Null) {
        body.as_object_mut().unwrap().remove("cursor");
    }
    body["kind"] = json!(operation);
    let frame = json!({"mode":"Query","protocol":PROTOCOL,"schemaRevision":revision,
        "queryId":format!("help-service-{}",app.n.fetch_add(1,Ordering::SeqCst)),"body":body});
    app.runtime
        .query(ctx, operation, frame.to_string().as_bytes())
        .unwrap()
}
fn result(reply: DomainQueryReply) -> Value {
    match reply {
        DomainQueryReply::Result { result, .. } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("{other:?}"),
    }
}
fn search(app: &HelpApp, ctx: &InvocationContext, limit: u16, cursor: Option<Value>) -> Value {
    result(query(
        app,
        ctx,
        "help.search",
        json!({"query":"*","page":{"limit":limit,"cursor":cursor}}),
        3,
    ))
}
fn bind(app: &HelpApp, name: &str, families: Vec<&str>, role: AgentRole) -> InvocationContext {
    let context = principal(&*app.store, name, PrincipalKind::Agent);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    let actor = context.principal().clone();
    let families = families.into_iter().map(str::to_owned).collect();
    write(&*app.store, move |tx| {
        bind_principal(
            tx,
            &user,
            NewPrincipalBinding {
                actor,
                kind: PrincipalKind::Agent,
                role: Some(role),
                project_id: None,
                tool_families: Some(families),
                bound_by: user.principal().clone(),
            },
            None,
        )
        .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))
    })
    .unwrap();
    context
}
fn artifact_read(app: &HelpApp, ctx: &InvocationContext, reference: &Value) -> DomainQueryReply {
    let frame = json!({"protocol":PROTOCOL,"queryId":format!("help-ref-{}",app.n.fetch_add(1,Ordering::SeqCst)),
        "query":{"kind":"Read","artifactId":reference["artifactId"],"versionId":reference["versionId"]},"limit":1});
    app.runtime
        .query(ctx, "artifacts.read", frame.to_string().as_bytes())
        .unwrap()
}
fn fault(reply: DomainQueryReply, code: devworks_core::protocol::DomainErrorCode) {
    let DomainQueryReply::Rejected { fault, .. } = reply else {
        panic!("expected fault: {reply:?}")
    };
    assert_eq!(fault.code, code);
}
fn finish(rt: &tokio::runtime::Runtime, app: HelpApp) {
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn help_only_agent_reads_inline_and_allowed_legacy_audiences_can_read_the_real_artifact() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let app = boot_help(&rt, &h);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    let help_only = bind(&app, "help-only", vec!["help"], AgentRole::Worker);
    let before = read(&*app.store, |s| Ok(s.stamp().committed_sequence));
    let page = search(&app, &help_only, 10, None);
    assert_eq!(page["availability"], "Available");
    assert_eq!(page["snapshot"]["canGenerate"], false);
    assert!(page["snapshot"]["cannotGenerateReason"].as_str().is_some());
    let item = &page["items"][0];
    let read_topic = result(query(
        &app,
        &help_only,
        "help.read",
        json!({"topicId":item["id"],"version":item["version"],"maxChars":"31"}),
        3,
    ));
    assert_eq!(read_topic["kind"], "Available");
    assert!(!read_topic["passage"]["text"].as_str().unwrap().is_empty());
    fault(
        artifact_read(&app, &help_only, &item["content"]),
        devworks_core::protocol::DomainErrorCode::PermissionDenied,
    );
    assert_eq!(
        before,
        read(&*app.store, |s| Ok(s.stamp().committed_sequence)),
        "queries do not publish Help/artifacts/history"
    );
    let legacy = result(query(
        &app,
        &user,
        "help.read",
        json!({"topicId":item["id"],"version":item["version"]}),
        1,
    ));
    assert!(legacy["content"]["artifactId"].is_string());
    let legacy_search = result(query(
        &app,
        &user,
        "help.search",
        json!({"query":"*","page":{"limit":10}}),
        1,
    ));
    assert!(
        legacy_search["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|topic| topic["content"]["artifactId"].is_string())
    );

    let artifact = result(artifact_read(&app, &user, &legacy["content"]));
    assert_eq!(artifact["kind"], "Text");
    assert_eq!(
        devworks_core::fingerprint::sha256_hex(artifact["content"].as_str().unwrap().as_bytes()),
        legacy["content"]["sha256"]
    );

    let coordinator = bind(
        &app,
        "help-coordinator",
        vec!["help", "artifacts"],
        AgentRole::WorkspaceCoordinator,
    );
    let coordinator_legacy = result(query(
        &app,
        &coordinator,
        "help.read",
        json!({"topicId":item["id"],"version":item["version"]}),
        1,
    ));
    assert!(matches!(
        artifact_read(&app, &coordinator, &coordinator_legacy["content"]),
        DomainQueryReply::Result { .. }
    ));
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let frame = json!({"protocol":PROTOCOL,"mode":"Command","schemaRevision":1,"requestKey":"agent-forbidden-generation",
        "body":{"kind":DOCS,"registryRevision":args.registry_revision,"expectedHelpRevision":args.expected_help_revision}});
    let reply = app
        .runtime
        .command(&help_only, DOCS, frame.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Rejected {
                code: devworks_core::protocol::Rejection::PermissionDenied,
                ..
            }
        ),
        "{reply:?}"
    );
    finish(&rt, app);
}

#[test]
fn help_pages_and_unicode_passages_are_exact_and_cursors_reject_cross_query_or_principal() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let app = boot_help(&rt, &h);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    let first = search(&app, &user, 7, None);
    let cursor = first["nextCursor"].clone();
    assert!(cursor.is_string());
    let second = search(&app, &user, 7, Some(cursor.clone()));
    let first_ids = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect::<Vec<_>>();
    assert!(
        second["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| !first_ids.contains(&item["id"]))
    );
    fault(
        query(
            &app,
            &user,
            "help.search",
            json!({"query":"SDK","page":{"limit":7,"cursor":cursor}}),
            3,
        ),
        devworks_core::protocol::DomainErrorCode::CursorExpired,
    );
    let other = bind(&app, "other-help", vec!["help"], AgentRole::Worker);
    fault(
        query(
            &app,
            &other,
            "help.search",
            json!({"query":"*","page":{"limit":7,"cursor":first["nextCursor"]}}),
            3,
        ),
        devworks_core::protocol::DomainErrorCode::CursorExpired,
    );
    let body = include_str!("../../../../docs/help/topics/help.md");
    let mut cursor = Value::Null;
    let mut collected = String::new();
    let mut offset = 0usize;
    loop {
        let page = result(query(
            &app,
            &user,
            "help.read",
            json!({"topicId":"help","version":env!("CARGO_PKG_VERSION"),"maxChars":"7","cursor":cursor}),
            3,
        ));
        assert_eq!(page["passage"]["offset"], offset.to_string());
        let text = page["passage"]["text"].as_str().unwrap();
        assert!(text.chars().count() <= 7);
        offset += text.chars().count();
        collected.push_str(text);
        if page["passage"]["hasMore"] == false {
            break;
        }
        cursor = page["passage"]["nextCursor"].clone();
    }
    assert_eq!(collected, body);
    fault(
        query(
            &app,
            &user,
            "help.read",
            json!({"topicId":"help","version":env!("CARGO_PKG_VERSION"),"maxChars":"8","cursor":first["nextCursor"]}),
            3,
        ),
        devworks_core::protocol::DomainErrorCode::CursorExpired,
    );
    finish(&rt, app);
}

#[test]
fn current_help_binds_real_screens_and_reports_missing_versions_and_packages_without_fake_refs() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let app = boot_help(&rt, &h);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    let page = result(query(
        &app,
        &user,
        "help.topic_for_screen",
        json!({"screenId":"help","current":true}),
        3,
    ));
    assert_eq!(page["items"][0]["id"], "help");
    assert_eq!(page["snapshot"]["canGenerate"], true);
    let missing = result(query(
        &app,
        &user,
        "help.topic_for_screen",
        json!({"screenId":"unregistered-screen","current":true}),
        3,
    ));
    assert_eq!(missing["availability"], "Missing");
    assert_eq!(missing["items"], json!([]));
    let stale = result(query(
        &app,
        &user,
        "help.read",
        json!({"topicId":"help","version":"old-app-version"}),
        3,
    ));
    assert_eq!(stale["availability"], "Stale");
    assert!(stale.get("content").is_none());
    let missing = result(query(
        &app,
        &user,
        "help.package_guides",
        json!({"packageId":"not-installed","version":"1.0.0","page":{"limit":10}}),
        3,
    ));
    assert_eq!(missing["availability"], "Missing");
    assert_eq!(missing["items"], json!([]));
    let legacy = result(query(
        &app,
        &user,
        "help.topic_for_screen",
        json!({"screenId":"help","capabilityRevision":page["snapshot"]["registryRevision"]}),
        1,
    ));
    assert!(legacy["items"][0]["content"]["artifactId"].is_string());
    finish(&rt, app);
}

fn refused(reply: DomainCommandReply) {
    assert!(
        matches!(reply,CommandReply::Recorded{receipt:devworks_core::protocol::Receipt{decision:DomainDecision::Refused{ref fault},..},..}
        if fault.code==devworks_core::protocol::DomainErrorCode::Conflict && fault.target_kind.as_ref().is_some_and(|kind|kind.0=="help_retained")),
        "{reply:?}"
    );
}
fn public_workspace_artifacts(
    app: &HelpApp,
    user: &InvocationContext,
    cursor: Option<&str>,
    tags: Option<Value>,
) -> Value {
    let mut frame = json!({"protocol":PROTOCOL,"schemaRevision":4,"queryId":"public-artifact-page",
        "query":{"kind":"List","workspace":true},"limit":1});
    if let Some(cursor) = cursor {
        frame["cursor"] = json!(cursor);
    }
    if let Some(tags) = tags {
        frame["query"]["tags"] = tags;
    }
    result(
        app.runtime
            .query(user, "artifacts.list", frame.to_string().as_bytes())
            .unwrap(),
    )
}

#[test]
fn help_artifacts_reject_public_mutation_while_ordinary_uploads_cannot_spoof_the_host_marker() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let app = boot_help(&rt, &h);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    let topic = result(query(
        &app,
        &user,
        "help.read",
        json!({"topicId":"help","version":env!("CARGO_PKG_VERSION")}),
        1,
    ));
    let reference = &topic["content"];
    let empty = public_workspace_artifacts(&app, &user, None, None);
    assert_eq!(
        empty["items"],
        json!([]),
        "bundled bodies are not user/work outputs: {empty}"
    );
    assert_eq!(
        empty["hasMore"], false,
        "hidden Help bodies cannot create a trailing page"
    );
    let artifact = id(reference["artifactId"].as_str().unwrap());
    let version = id(reference["versionId"].as_str().unwrap());
    let original = read(&*app.store, |s| s.artifacts().artifact(&artifact)).unwrap();
    refused(app.command("artifacts.tag",json!({"artifactId":artifact,"expectedRevision":original.revision.to_string(),"tags":["altered"]}),1));
    let request = json!({"protocol":PROTOCOL,"requestKey":"delete-help-version","deletion":{"kind":"DeleteVersion","artifactId":artifact,"versionId":version,
        "expectedRevision":original.revision.to_string(),"expectedReferenceRevision":original.reference_revision.to_string()}});
    refused(
        app.runtime
            .command(&user, "artifacts.delete", request.to_string().as_bytes())
            .unwrap(),
    );
    let request = json!({"protocol":PROTOCOL,"requestKey":"delete-help-artifact","deletion":{"kind":"DeleteArtifact","artifactId":artifact,
        "expectedRevision":original.revision.to_string(),"expectedReferenceRevision":original.reference_revision.to_string(),"expectedVersionsDigest":"a".repeat(64)}});
    refused(
        app.runtime
            .command(&user, "artifacts.delete", request.to_string().as_bytes())
            .unwrap(),
    );
    let project=value(app.command("projects.create",json!({"name":"Help retention scope","description":"Actual project for share refusal","repositoryIds":[]}),1));
    let project = devworks_core::protocol::ProjectId(id(project["id"].as_str().unwrap()));
    let service = app.runtime.artifacts().clone();
    let ctx = user.clone();
    let target = artifact.clone();
    let destination = project.clone();
    let denied=write(&*app.store,move|tx|Ok(matches!(service.share(tx,&ctx,&target,&destination,&Revision::new(1).unwrap()),Err(devworks_core::services::artifacts::Failure::Refused(refusal)) if refusal.target_kind=="help_retained"))).unwrap();
    assert!(denied, "document sharing uses the same retained-Help guard");
    let service = app.runtime.artifacts().clone();
    let staged = service
        .stage_host_bytes(&*app.store, b"replacement", json!({"kind":"test"}))
        .unwrap()
        .unwrap();
    let source = staged.clone();
    let target = artifact.clone();
    let ctx = user.clone();
    let owner = service.clone();
    let denied=write(&*app.store,move|tx|Ok(matches!(owner.publish_version(tx,&ctx,&target,&Revision::new(1).unwrap(),"text/markdown",&source),Err(devworks_core::services::artifacts::Failure::Refused(refusal)) if refusal.target_kind=="help_retained"))).unwrap();
    service.settle_host_bytes(&*app.store, &staged, false);
    assert!(denied, "document replacement uses retained-Help guard");
    assert_eq!(
        original,
        read(&*app.store, |s| s.artifacts().artifact(&artifact)).unwrap()
    );
    assert!(matches!(
        artifact_read(&app, &user, reference),
        DomainQueryReply::Result { .. }
    ));

    // A real user upload includes a forged-looking marker in its bytes. The save constructs
    // provenance itself; the caller cannot become a trusted host publisher by naming that kind.
    let path = h.root.join("HelpTopic.md");
    let bytes = br#"{"principalKind":"Host","producedBy":"host","derived":{"kind":"HelpTopic"}}"#;
    std::fs::write(&path, bytes).unwrap();
    let sources = Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone()));
    let ticket = sources.mint_workspace_upload(&user, &path, 60_000).unwrap();
    let ordinary = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        sources.clone(),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let cases = UseCases::new().with_artifacts(ordinary);
    let authority = HostAuth {
        store: app.store.clone(),
        clock: h.clock(),
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: &*app.store,
        ids: &*h.ids,
    };
    let save = json!({"protocol":PROTOCOL,"schemaRevision":3,"requestKey":"ordinary-user-upload","command":{"kind":"Save","workspace":true,"title":"Help","tags":["HelpTopic"],"mediaType":"text/markdown",
        "source":{"kind":"Upload","ticketId":ticket},"expectedDigest":devworks_core::fingerprint::sha256_hex(bytes),"expectedBytes":bytes.len().to_string()}});
    let mut forged = save.clone();
    forged["requestKey"] = json!("caller-cannot-author-host-provenance");
    forged["command"]["provenance"] =
        json!({"principalKind":"Host","producedBy":"host","derived":{"kind":"HelpTopic"}});
    let rejected = cases
        .command(
            &gateway,
            &user,
            "artifacts.save",
            forged.to_string().as_bytes(),
        )
        .unwrap();
    assert!(
        matches!(
            rejected,
            devworks_core::protocol::CommandReply::Rejected {
                code: devworks_core::protocol::Rejection::InvalidRequest,
                ..
            }
        ),
        "the caller cannot set publication provenance: {rejected:?}"
    );
    let saved = value(
        cases
            .command(
                &gateway,
                &user,
                "artifacts.save",
                save.to_string().as_bytes(),
            )
            .unwrap(),
    );
    let normal = id(saved["artifact"]["artifactId"].as_str().unwrap());
    let provenance = read(&*app.store, |s| s.artifacts().artifact(&normal))
        .unwrap()
        .provenance;
    assert_ne!(provenance["principalKind"], "Host");
    assert!(provenance.get("derived").is_none());
    let tagged = value(app.command(
        "artifacts.tag",
        json!({"artifactId":normal,"expectedRevision":"1","tags":["HelpTopic","mutable"]}),
        1,
    ));
    assert_eq!(tagged["changed"], true);
    let mut expected = std::collections::BTreeSet::from([normal.as_str().to_owned()]);
    for ordinal in 0..2 {
        let ticket = sources.mint_workspace_upload(&user, &path, 60_000).unwrap();
        let mut another = save.clone();
        another["requestKey"] = json!(format!("lookalike-upload-{ordinal}"));
        another["command"]["source"]["ticketId"] = json!(ticket);
        another["command"]["title"] = json!(format!("HelpTopic {ordinal}"));
        let next = value(
            cases
                .command(
                    &gateway,
                    &user,
                    "artifacts.save",
                    another.to_string().as_bytes(),
                )
                .unwrap(),
        );
        expected.insert(next["artifact"]["artifactId"].as_str().unwrap().to_owned());
    }
    // Other host-produced work outputs remain ordinary artifacts, even with a Help-like
    // title/tag and no derived.kind field. SQL NULL must not classify these as Help bodies.
    let service = app.runtime.artifacts().clone();
    let staged = service
        .stage_host_bytes(&*app.store, b"work output", json!({"kind":"WorkOutput"}))
        .unwrap()
        .unwrap();
    let pending = staged.clone();
    let owner = service.clone();
    let output = write(&*app.store, move |tx| {
        owner.commit_host_bytes(tx, &pending, "HelpTopic", "text/plain", json!({}))
    })
    .unwrap();
    service.settle_host_bytes(&*app.store, &staged, true);
    let report = output.artifact_id;
    value(app.command(
        "artifacts.tag",
        json!({"artifactId":report,"expectedRevision":"1","tags":["HelpTopic"]}),
        1,
    ));
    expected.insert(report.as_str().to_owned());
    for tags in [None, Some(json!(["HelpTopic"]))] {
        let mut seen = std::collections::BTreeSet::new();
        let mut cursor = None;
        loop {
            let page = public_workspace_artifacts(&app, &user, cursor.as_deref(), tags.clone());
            let items = page["items"].as_array().unwrap();
            assert_eq!(
                items.len(),
                1,
                "page bound applies after Help exclusion: {page}"
            );
            assert!(
                seen.insert(
                    items[0]["artifact"]["artifactId"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                )
            );
            assert!(
                seen.len() <= expected.len(),
                "system Help cannot leak into work pages"
            );
            if page["hasMore"] == false {
                assert!(page["nextCursor"].is_null());
                break;
            }
            cursor = Some(page["nextCursor"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            seen, expected,
            "user names, tags and forged-looking bytes never hide uploads"
        );
    }
    assert!(
        matches!(
            artifact_read(&app, &user, reference),
            DomainQueryReply::Result { .. }
        ),
        "the old Help artifact reference remains readable after public listing"
    );
    finish(&rt, app);
}

#[test]
fn concurrent_generation_never_reports_someone_elses_pending_effect_as_completed() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let app = boot_help(&rt, &h);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    app.runtime.dispatcher().stop();
    rt.block_on(app.runtime.dispatcher().idle());
    let before = app.head();
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let first = app.command(DOCS, serde_json::to_value(&args).unwrap(), 1);
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision: DomainDecision::Accepted { operation_id },
                ..
            },
        ..
    } = first
    else {
        panic!("{first:?}")
    };
    let after = app.head();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.generation_id, before.generation_id);
    assert_eq!(after.generation_operation_id, Some(operation_id.clone()));
    let second = app.command(DOCS, serde_json::to_value(args).unwrap(), 1);
    assert!(
        matches!(
            second,
            CommandReply::Rejected {
                code: devworks_core::protocol::Rejection::StorageUnavailable,
                retry: devworks_core::protocol::RetryAdvice::SameCommand,
                ..
            }
        ),
        "{second:?}"
    );
    assert_eq!(app.head(), after);
    let snapshot = search(&app, &user, 1, None);
    assert_eq!(
        snapshot["snapshot"]["generationOperation"]["operationId"],
        json!(operation_id)
    );
    assert_ne!(
        snapshot["snapshot"]["generationOperation"]["state"],
        "ObservedComplete"
    );
    finish(&rt, app);
}

#[test]
fn unchanged_regeneration_preserves_topic_references_and_exact_receipt_replay() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let app = boot_help(&rt, &h);
    let user = principal(&*app.store, "user-a", PrincipalKind::Human);
    let old = app.head();
    let old_page = search(&app, &user, 3, None);
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let frame=json!({"protocol":PROTOCOL,"mode":"Command","schemaRevision":1,"requestKey":"exact-help-no-op",
        "body":{"kind":DOCS,"registryRevision":args.registry_revision,"expectedHelpRevision":args.expected_help_revision}}).to_string();
    let original = app.runtime.command(&user, DOCS, frame.as_bytes()).unwrap();
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision: DomainDecision::Accepted { ref operation_id },
                ..
            },
        ..
    } = original
    else {
        panic!("{original:?}")
    };
    rt.block_on(within("unchanged Help regeneration", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            let record = read(&*app.store, |s| s.operations().operation(operation_id)).unwrap();
            if is_terminal(record.state) {
                assert_eq!(record.state, OperationState::ObservedComplete, "{record:?}");
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let head = app.head();
    assert_eq!(head.revision, old.revision);
    assert_eq!(head.generation_id, old.generation_id);
    assert_eq!(head.source_digest, old.source_digest);
    let new_page = search(&app, &user, 3, None);
    assert_eq!(old_page["items"], new_page["items"]);
    assert_eq!(
        new_page["snapshot"]["generationOperation"]["state"],
        "ObservedComplete"
    );
    let replay = app.runtime.command(&user, DOCS, frame.as_bytes()).unwrap();
    let CommandReply::Recorded {
        receipt, replayed, ..
    } = replay
    else {
        panic!("{replay:?}")
    };
    assert!(replayed);
    assert!(
        matches!(receipt.decision,DomainDecision::Accepted{operation_id:ref same} if same==operation_id)
    );
    rt.block_on(within("bounded unchanged-draft cleanup", async {
        loop {
            if read(&*app.store, |s| s.help().help_drafts(None, 32)).is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    for item in new_page["items"].as_array().unwrap() {
        assert!(
            matches!(
                artifact_read(&app, &user, &item["content"]),
                DomainQueryReply::Result { .. }
            ),
            "cleanup retains reused published references"
        );
    }
    let continued = search(&app, &user, 3, Some(old_page["nextCursor"].clone()));
    assert!(!continued["items"].as_array().unwrap().is_empty());
    finish(&rt, app);
}

#[test]
fn missing_or_corrupt_generated_source_can_be_rebuilt_without_losing_other_help() {
    let _native_case = crate::common::native_runtime_case();
    for missing in [true, false] {
        let rt = runtime();
        let h = Harness::new();
        let app = boot_help(&rt, &h);
        let _user = principal(&*app.store, "user-a", PrincipalKind::Human);
        let topic_id = id(&format!(
            "capability-{}",
            &devworks_core::fingerprint::sha256_hex(b"help.search\0Query")[..40]
        ));
        let old_head = app.head();
        let old = read(&*app.store, |s| {
            s.help()
                .help_topic(old_head.generation_id.as_ref().unwrap(), &topic_id)
        })
        .unwrap();
        let reference: devworks_core::protocol::provider::ArtifactRef =
            serde_json::from_value(old.content_ref.clone().unwrap()).unwrap();
        let version = read(&*app.store, |s| {
            s.artifacts().version(&reference.version_id)
        })
        .unwrap();
        let digest = reference.sha256.as_str();
        let path = h
            .root
            .join("artifacts")
            .join("sha256")
            .join(&digest[..2])
            .join(format!("{digest}.{}", version.blob_generation));
        assert!(path.is_file());
        if missing {
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::write(&path, b"damaged generated source").unwrap();
        }
        let args = read(
            &*app.store,
            devworks_core::services::help::current_generation_args,
        );
        let reply = app.command(DOCS, serde_json::to_value(args).unwrap(), 1);
        let CommandReply::Recorded {
            receipt:
                devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { operation_id },
                    ..
                },
            ..
        } = reply
        else {
            panic!("{reply:?}");
        };
        rt.block_on(within("repair generated Help source", async {
            loop {
                app.runtime.dispatcher().run_until_idle().await.unwrap();
                let op = read(&*app.store, |s| s.operations().operation(&operation_id)).unwrap();
                if is_terminal(op.state) {
                    assert_eq!(
                        op.state,
                        OperationState::ObservedComplete,
                        "missing={missing}: {op:?}"
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        }));
        let head = app.head();
        let repaired = read(&*app.store, |s| {
            s.help()
                .help_topic(head.generation_id.as_ref().unwrap(), &topic_id)
        })
        .unwrap();
        assert_ne!(repaired.content_ref, old.content_ref);
        assert_eq!(
            read(&*app.store, |s| s
                .help()
                .help_topic(&old.generation_id, &topic_id))
            .unwrap(),
            old
        );
        let page = result(query(
            &app,
            &app.user(),
            "help.read",
            json!({"topicId":topic_id,"version":repaired.version,"maxChars":"65536"}),
            3,
        ));
        assert_eq!(page["kind"], "Available");
        let text = page["passage"]["text"].as_str().unwrap();
        assert!(text.contains("help.search"));
        assert!(text.contains("manual material") && text.contains("could not be recovered"));
        assert_eq!(
            result(query(
                &app,
                &app.user(),
                "help.read",
                json!({"topicId":"help","version":env!("CARGO_PKG_VERSION")}),
                3
            ))["kind"],
            "Available"
        );
        finish(&rt, app);
    }
}

struct PublicationAckLoss {
    stage_after: Option<u16>,
    fail_read: Arc<AtomicBool>,
    inner: Arc<dyn ApplicationStore>,
    armed: Arc<AtomicBool>,
    lost: Arc<AtomicUsize>,
}
struct PublicationJob {
    stage_after: Option<u16>,
    inner: Box<dyn WriteJob>,
    armed: Arc<AtomicBool>,
    lost: Arc<AtomicUsize>,
    hide: bool,
}
impl WriteJob for PublicationJob {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        let before = tx.help().help_head()?.revision;
        self.inner.run(tx)?;
        let hit = match self.stage_after {
            None => tx.help().help_head()?.revision > before,
            Some(count) => tx
                .help()
                .help_drafts(None, 1)?
                .first()
                .map(|draft| {
                    tx.help()
                        .staged_help_topics(&draft.generation_id, None, count)
                        .map(|rows| rows.len() >= usize::from(count))
                })
                .transpose()?
                .unwrap_or(false),
        };
        if hit && self.armed.swap(false, Ordering::SeqCst) {
            self.hide = true;
        }
        Ok(())
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        if self.hide && outcome.is_ok() {
            self.lost.fetch_add(1, Ordering::SeqCst);
            self.inner.finish(Err(StorageError::CommitUnknown));
        } else {
            self.inner.finish(outcome);
        }
    }
}
impl ApplicationStore for PublicationAckLoss {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        if self.fail_read.swap(false, Ordering::SeqCst) {
            return Err(StorageError::unavailable(
                Unavailable::Io,
                "injected read interruption",
            ));
        }
        self.inner.read(options, work)
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.inner.submit_write(
            options,
            Box::new(PublicationJob {
                stage_after: self.stage_after,
                inner: job,
                armed: self.armed.clone(),
                lost: self.lost.clone(),
                hide: false,
            }),
        )
    }
}
#[test]
fn lost_catalog_publication_ack_recovers_from_one_exact_durable_observation() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let loss = Arc::new(PublicationAckLoss {
        stage_after: None,
        fail_read: Arc::new(AtomicBool::new(false)),
        inner: h.open(),
        armed: Arc::new(AtomicBool::new(true)),
        lost: Arc::new(AtomicUsize::new(0)),
    });
    let store: Arc<dyn ApplicationStore> = loss.clone();
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let service = Arc::new(devworks_core::services::help::HelpService::new(
        store.clone(),
        artifacts,
        h.ids.clone(),
        h.clock(),
        None,
    ));
    let cases = UseCases::new().with_help(service.clone());
    service.set_composition(&cases);
    let authority = HostAuth {
        store: store.clone(),
        clock: h.clock(),
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let (context, frame) = service.maintenance_request().unwrap().unwrap();
    let reply = cases.command(&gateway, &context, DOCS, &frame).unwrap();
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision: DomainDecision::Accepted { operation_id },
                ..
            },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    let dispatcher = dispatcher_with(
        &h,
        store.clone(),
        vec![devworks_engine::help::consumer(service.clone())],
        |_| {},
    );
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        loss.lost.load(Ordering::SeqCst),
        1,
        "only the actual head publication acknowledgment was lost"
    );
    let head = read(&*store, |s| s.help().help_head());
    assert!(head.generation_id.is_some());
    let operation = read(&*store, |s| s.operations().operation(&operation_id)).unwrap();
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
    let generation = head.generation_id.clone().unwrap();
    let topics = read(&*store, |s| {
        s.help()
            .help_topics(&generation, &HelpFilter::default(), None, 500)
    });
    let published = read(&*store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "help".into(),
                id: "catalog".into(),
            },
            0,
            500,
        )
    });
    assert_eq!(
        published
            .events
            .iter()
            .filter(|event| event.kind == "HelpGenerationPublished")
            .count(),
        1
    );
    loss.fail_read.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            service.observe(&operation),
            devworks_core::services::help::GenerationOutcome::Unknown(_)
        ),
        "an unreadable journal cannot be replaced by a new build"
    );
    assert_eq!(rt.block_on(dispatcher.observe_unknown()).unwrap(), 1);
    let operation = read(&*store, |s| s.operations().operation(&operation_id)).unwrap();
    assert_eq!(operation.state, OperationState::ObservedComplete);
    assert_eq!(head, read(&*store, |s| s.help().help_head()));
    assert_eq!(
        topics,
        read(&*store, |s| s.help().help_topics(
            &generation,
            &HelpFilter::default(),
            None,
            500
        ))
    );
    let published = read(&*store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "help".into(),
                id: "catalog".into(),
            },
            0,
            500,
        )
    });
    assert_eq!(
        published
            .events
            .iter()
            .filter(|event| event.kind == "HelpGenerationPublished")
            .count(),
        1,
        "reconciliation did not regenerate or publish another artifact set"
    );
    assert_eq!(rt.block_on(dispatcher.observe_unknown()).unwrap(), 0);
}

fn core_help(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
) -> (
    Arc<devworks_core::services::help::HelpService>,
    UseCases,
    devworks_engine::Dispatcher,
) {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let service = Arc::new(devworks_core::services::help::HelpService::new(
        store.clone(),
        artifacts,
        h.ids.clone(),
        h.clock(),
        None,
    ));
    let cases = UseCases::new().with_help(service.clone());
    service.set_composition(&cases);
    let dispatcher = dispatcher_with(
        h,
        store,
        vec![devworks_engine::help::consumer(service.clone())],
        |_| {},
    );
    (service, cases, dispatcher)
}
#[test]
fn a_restart_resumes_only_the_original_invisible_draft_and_preserves_prepared_refs() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let (operation_id, generation, prepared) = {
        let store: Arc<dyn ApplicationStore> = Arc::new(PublicationAckLoss {
            stage_after: Some(2),
            fail_read: Arc::new(AtomicBool::new(false)),
            inner: h.open(),
            armed: Arc::new(AtomicBool::new(true)),
            lost: Arc::new(AtomicUsize::new(0)),
        });
        let (service, cases, dispatcher) = core_help(&h, store.clone());
        let authority = HostAuth {
            store: store.clone(),
            clock: h.clock(),
        };
        let gateway = devworks_core::gateway::Gateway {
            authority: &authority,
            store: &*store,
            ids: &*h.ids,
        };
        let (context, frame) = service.maintenance_request().unwrap().unwrap();
        let reply = cases.command(&gateway, &context, DOCS, &frame).unwrap();
        let CommandReply::Recorded {
            receipt:
                devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { operation_id },
                    ..
                },
            ..
        } = reply
        else {
            panic!("{reply:?}")
        };
        rt.block_on(dispatcher.run_until_idle()).unwrap();
        let head = read(&*store, |s| s.help().help_head());
        assert!(
            head.generation_id.is_none(),
            "partial bodies never become a visible catalog"
        );
        let operation = read(&*store, |s| s.operations().operation(&operation_id)).unwrap();
        assert_eq!(operation.state, OperationState::OutcomeUnknown);
        let generation: Id =
            serde_json::from_value(operation.payload["generation"].clone()).unwrap();
        let source = generation.clone();
        let prepared = write(&*store, move |tx| {
            tx.help().staged_help_topics(&source, None, 500)
        })
        .unwrap();
        assert_eq!(prepared.len(), 2);
        (operation_id, generation, prepared)
    };
    let store = h.open();
    let (service, _cases, dispatcher) = core_help(&h, store.clone());
    rt.block_on(dispatcher.recover()).unwrap();
    assert_eq!(rt.block_on(dispatcher.observe_unknown()).unwrap(), 1);
    let head = read(&*store, |s| s.help().help_head());
    assert_eq!(head.generation_id, Some(generation.clone()));
    assert_eq!(head.generation_operation_id, Some(operation_id.clone()));
    let operation = read(&*store, |s| s.operations().operation(&operation_id)).unwrap();
    assert_eq!(operation.state, OperationState::ObservedComplete);
    for topic in prepared {
        assert_eq!(
            Some(topic.clone()),
            read(&*store, |s| s.help().help_topic(&generation, &topic.id))
        );
    }
    assert!(
        service.maintenance_request().unwrap().is_none(),
        "recovery does not mint another generation"
    );
}

#[test]
fn a_cold_source_change_does_not_strand_the_next_generation_behind_a_failed_partition() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let store = h.open();
    let (service, cases, dispatcher) = core_help(&h, store.clone());
    let authority = HostAuth {
        store: store.clone(),
        clock: h.clock(),
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let (context, frame) = service.maintenance_request().unwrap().unwrap();
    let first = cases.command(&gateway, &context, DOCS, &frame).unwrap();
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision:
                    DomainDecision::Accepted {
                        operation_id: first,
                    },
                ..
            },
        ..
    } = first
    else {
        panic!("{first:?}")
    };
    // The real registry revision changes after startup admission and before its local effect.
    // This is the same fence transition as an installed/disabled package during cold startup.
    write(&*store, |tx| tx.packages().advance_registry(1)).unwrap();
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        read(&*store, |s| s.operations().operation(&first))
            .unwrap()
            .state,
        OperationState::Failed
    );
    assert!(
        read(&*store, |s| s.help().help_head())
            .generation_id
            .is_none()
    );
    let (context, frame) = service.maintenance_request().unwrap().unwrap();
    let second = cases.command(&gateway, &context, DOCS, &frame).unwrap();
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision:
                    DomainDecision::Accepted {
                        operation_id: second,
                    },
                ..
            },
        ..
    } = second
    else {
        panic!("{second:?}")
    };
    assert_ne!(first, second);
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        read(&*store, |s| s.operations().operation(&second))
            .unwrap()
            .state,
        OperationState::ObservedComplete
    );
    assert_eq!(read(&*store, |s| s.help().help_head()).registry_revision, 2);
}

#[test]
fn cancelled_unpublished_drafts_are_cleaned_in_bounded_pages_without_touching_published_refs() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let store: Arc<dyn ApplicationStore> = Arc::new(PublicationAckLoss {
        stage_after: Some(2),
        fail_read: Arc::new(AtomicBool::new(false)),
        inner: h.open(),
        armed: Arc::new(AtomicBool::new(true)),
        lost: Arc::new(AtomicUsize::new(0)),
    });
    let (service, cases, dispatcher) = core_help(&h, store.clone());
    let authority = HostAuth {
        store: store.clone(),
        clock: h.clock(),
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let (context, frame) = service.maintenance_request().unwrap().unwrap();
    let reply = cases.command(&gateway, &context, DOCS, &frame).unwrap();
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision: DomainDecision::Accepted { operation_id },
                ..
            },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    let operation = read(&*store, |s| s.operations().operation(&operation_id)).unwrap();
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
    let generation: Id = serde_json::from_value(operation.payload["generation"].clone()).unwrap();
    let delivery: Id = serde_json::from_value(operation.payload["deliveryId"].clone()).unwrap();
    let source = generation.clone();
    let prepared = write(&*store, move |tx| {
        tx.help().staged_help_topics(&source, None, 500)
    })
    .unwrap();
    assert_eq!(prepared.len(), 2);
    // Exact owner delivery cancellation is the same protocol the native jobs.cancel bridge
    // performs after its real Accepted receipt. No fabricated completion or replacement ID.
    write(&*store, move |tx| tx.queue().cancel(&delivery)).unwrap();
    assert_eq!(rt.block_on(dispatcher.observe_unknown()).unwrap(), 1);
    assert_eq!(
        read(&*store, |s| s.operations().operation(&operation_id))
            .unwrap()
            .state,
        OperationState::Failed
    );
    assert_eq!(service.cleanup().unwrap(), 2);
    assert_eq!(service.cleanup().unwrap(), 0);
    assert!(read(&*store, |s| s.help().help_drafts(None, 32)).is_empty());
    assert!(
        read(&*store, |s| s.help().help_head())
            .generation_id
            .is_none()
    );
    for topic in prepared {
        let reference = topic.content_ref.unwrap();
        let artifact = id(reference["artifactId"].as_str().unwrap());
        let version = id(reference["versionId"].as_str().unwrap());
        assert_eq!(
            read(&*store, |s| s.artifacts().artifact(&artifact))
                .unwrap()
                .state,
            ArtifactState::Deleted
        );
        assert!(
            !read(&*store, |s| s.artifacts().version(&version))
                .unwrap()
                .live()
        );
    }
}

#[test]
fn composed_capability_changes_remove_only_orphan_auto_topics_and_keep_authored_guides() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let store = h.open();
    let (service, base, dispatcher) = core_help(&h, store.clone());
    let authority = HostAuth {
        store: store.clone(),
        clock: h.clock(),
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let rich = base.clone().with_queue();
    service.set_composition(&rich);
    let (context, frame) = service.maintenance_request().unwrap().unwrap();
    let reply = rich.command(&gateway, &context, DOCS, &frame).unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { .. },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    let old = read(&*store, |s| s.help().help_head());
    let old_generation = old.generation_id.unwrap();
    let old_topics = read(&*store, |s| {
        s.help()
            .help_topics(&old_generation, &HelpFilter::default(), None, 500)
    });
    let authored = old_topics
        .iter()
        .filter(|topic| topic.source_kind == "Bundled")
        .cloned()
        .collect::<Vec<_>>();
    let catalog: Vec<Value> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/help/catalog.json"
    )))
    .unwrap();
    let expected: std::collections::BTreeSet<_> = catalog
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert!(expected.contains("help"));
    assert_eq!(
        authored
            .iter()
            .map(|topic| topic.id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    assert!(old_topics.iter().any(|topic| {
        topic.source_kind == "Generated"
            && topic
                .related_operations
                .iter()
                .any(|operation| operation.starts_with("queue."))
    }));
    service.set_composition(&base);
    let (context, frame) = service.maintenance_request().unwrap().unwrap();
    let reply = base.command(&gateway, &context, DOCS, &frame).unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { .. },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    let new = read(&*store, |s| s.help().help_head());
    assert!(new.revision > old.revision);
    let generation = new.generation_id.unwrap();
    let topics = read(&*store, |s| {
        s.help()
            .help_topics(&generation, &HelpFilter::default(), None, 500)
    });
    assert!(!topics.iter().any(|topic| {
        topic.source_kind == "Generated"
            && topic
                .related_operations
                .iter()
                .any(|operation| operation.starts_with("queue."))
    }));
    for old in authored {
        let now = topics.iter().find(|topic| topic.id == old.id).unwrap();
        assert_eq!(now.content_digest, old.content_digest);
        assert_eq!(now.content_ref, old.content_ref);
        assert_eq!(now.topic_revision, old.topic_revision);
    }
    assert_eq!(
        old_topics,
        read(&*store, |s| s.help().help_topics(
            &old_generation,
            &HelpFilter::default(),
            None,
            500
        )),
        "published history remains immutable"
    );
}

#[path = "help_errors.rs"]
mod error_classification;
