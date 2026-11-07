//! Help reads exact installed signed sources without activating a package or trusting a path.
use super::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock,
};
use devworks_core::protocol::OperationState;

struct HelpAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}
impl AuthorityPort for HelpAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        devworks_core::authority::HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
fn app() -> App {
    let mut app = App::unbooted(Harness::new(), None, None);
    let mut composition = app.composition();
    composition.authority = Arc::new(HelpAuthority {
        store: app.store.clone(),
        clock: app.h.clock(),
    });
    app.app = Some(app.rt.block_on(boot(composition)).unwrap());
    settled(&app);
    app
}
fn settled(app: &App) {
    let _diagnostics = crate::common::help_startup_diagnostics(&*app.store, app.app().dispatcher());
    app.rt
        .block_on(common::within("real Help publication", async {
            loop {
                app.app().dispatcher().run_until_idle().await.unwrap();
                let ready = read(&*app.store, |s| {
                    let head = s.help().help_head()?;
                    let terminal = head
                        .generation_operation_id
                        .as_ref()
                        .map(|id| s.operations().operation(id))
                        .transpose()?
                        .flatten()
                        .is_some_and(|operation| {
                            operation.state == OperationState::ObservedComplete
                        });
                    Ok(head.generation_id.is_some()
                        && terminal
                        && head.registry_revision == s.packages().registry_revision()?)
                });
                if ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }));
}
fn regenerate(app: &App) {
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let frame = json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":1,"requestKey":app.key("help-regenerate"),
        "body":{"kind":"help.generate_capability_docs","registryRevision":args.registry_revision,"expectedHelpRevision":args.expected_help_revision}});
    let reply = app
        .app()
        .command(
            &app.user,
            "help.generate_capability_docs",
            frame.to_string().as_bytes(),
        )
        .unwrap();
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
    settled(app);
}
fn query(app: &App, operation: &str, mut body: Value, revision: u32) -> Value {
    body["kind"] = json!(operation);
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":revision,"queryId":app.key("help-query"),"body":body});
    match app
        .app()
        .query(&app.user, operation, frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("{other:?}"),
    }
}
fn guides(app: &App, version: &str) -> Value {
    query(
        app,
        "help.package_guides",
        json!({"packageId":"guide-pack","version":version,"page":{"limit":100}}),
        3,
    )
}
fn guide(page: &Value) -> &Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|topic| topic["id"].as_str().unwrap().starts_with("package-guide-"))
        .unwrap()
}
fn install(app: &App, version: &str) -> PackageRecord {
    let spec = Spec {
        targets: vec!["riscv64gc-unknown-none-elf".into()],
        ..Spec::new("guide-pack", version)
    };
    let record = if version == "1.0.0" {
        package(app.install(&app.user, &spec))
    } else {
        package(app.update(&app.user, &spec))
    };
    assert!(matches!(
        record.availability,
        Some(PackageAvailability::Incompatible)
    ));
    assert!(
        app.app()
            .packages()
            .unwrap()
            .instance_status("guide-pack")
            .iter()
            .all(|(_, status)| *status != InstanceStatus::Ready)
    );
    record
}
#[test]
fn installed_help_is_manifest_bound_and_missing_tampered_or_linked_bytes_never_gain_a_ref() {
    let _native_case = crate::common::native_runtime_case();
    let _serial = serial();
    let app = app();
    let record = install(&app, "1.0.0");
    settled(&app);
    let first = guides(&app, "1.0.0");
    let item = guide(&first);
    assert_eq!(item["availability"], "Available");
    assert_eq!(item["provenance"]["manifestDigest"], json!(record.digest));
    assert!(item["reason"].as_str().unwrap().contains("Incompatible"));
    let body = query(
        &app,
        "help.read",
        json!({"topicId":item["id"],"version":"1.0.0"}),
        3,
    );
    assert_eq!(body["passage"]["text"], "# guide-pack\n");
    assert!(
        first["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|topic| topic["id"]
                .as_str()
                .unwrap()
                .starts_with("package-contract-")),
        "actual installed contribution declarations are documented separately"
    );
    let file = app
        .h
        .root
        .join("packages")
        .join("guide-pack")
        .join(String::from(record.digest.clone()))
        .join("help.md");
    std::fs::write(&file, b"unapproved guide").unwrap();
    regenerate(&app);
    let tampered = guides(&app, "1.0.0");
    let item = guide(&tampered);
    assert_eq!(item["availability"], "Unavailable");
    assert!(item.get("content").is_none());
    std::fs::remove_file(&file).unwrap();
    regenerate(&app);
    let missing = guides(&app, "1.0.0");
    let item = guide(&missing);
    assert_eq!(item["availability"], "Missing");
    assert!(item.get("content").is_none());
    let outside = app.h.root.join("outside-guide.md");
    std::fs::write(&outside, b"# guide-pack\n").unwrap();
    std::fs::hard_link(&outside, &file).unwrap();
    regenerate(&app);
    let linked = guides(&app, "1.0.0");
    let item = guide(&linked);
    assert_eq!(item["availability"], "Unavailable");
    assert!(
        item.get("content").is_none(),
        "same-byte external hard link is not an approved file identity"
    );
    std::fs::remove_file(&file).unwrap();
    std::fs::write(&file, b"# guide-pack\n").unwrap();
    regenerate(&app);
    let repaired = guides(&app, "1.0.0");
    assert_eq!(guide(&repaired)["availability"], "Available");
    let legacy = query(
        &app,
        "help.package_guides",
        json!({"packageId":"guide-pack","version":"1.0.0","page":{"limit":100}}),
        1,
    );
    assert!(
        legacy["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|topic| topic["content"]["artifactId"].is_string())
    );
    let changed = install(&app, "2.0.0");
    settled(&app);
    let stale = guides(&app, "1.0.0");
    assert_eq!(stale["availability"], "Stale");
    assert_eq!(stale["items"], json!([]));
    let current = guides(&app, "2.0.0");
    assert_eq!(
        guide(&current)["provenance"]["manifestDigest"],
        json!(changed.digest)
    );
    package(app.simple(&app.user, "addons.uninstall", "Uninstall", "guide-pack"));
    let absent = guides(&app, "2.0.0");
    assert_eq!(absent["availability"], "Missing");
    assert_eq!(absent["items"], json!([]));
}

/// Exercise a legal source near the package bound; setup allocations are deliberately outside
/// measurement. Every actual Help hash, index, publication, read and search is measured.
#[test]
fn near_limit_guide_is_bounded_full_tail_unicode_and_reused_across_registry_churn() {
    let _native_case = crate::common::native_runtime_case();
    use devworks_core::ports::files::ArtifactFiles;
    use devworks_core::services::cursor::CursorBinding;
    use std::io::{Read, Seek, SeekFrom, Write};
    let _serial = serial();
    let app = app();
    // This is the actual supported storage setting, not a bypass of artifact admission.
    let author = app.user.principal().clone();
    write(&*app.store, move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let key = devworks_core::settings::catalog::ARTIFACT_MAX_FILE_KEY;
        let schema = serde_json::to_value(descriptor(key).unwrap().schema_ref()).unwrap();
        let value = serde_json::from_value(json!({"key":key,"value":{"schema":schema,"value":devworks_core::ports::packages::MAX_BUNDLE_BYTES}})).unwrap();
        let plan = model::plan(&current, &[value]);
        let body = model::body(&plan, &author, tx.now_ms(), json!({"kind":"test"}));
        tx.configuration().append_revision(&ConfigScope::Workspace, model::NAMESPACE, current.revision, model::BODY_SCHEMA_VERSION, body)?;
        Ok(())
    }).unwrap();
    let guide_path = app.h.root.join("large-approved-guide.md");
    let ascii_bytes = devworks_core::ports::packages::MAX_BUNDLE_BYTES as usize
        - helper_bytes().len()
        - 2 * 1024 * 1024;
    let tail = "\nFULL-TAIL-ÉVIDENCE: Straße İ ΟΣ́! 終端🦀\n";
    let mut file = std::fs::File::create(&guide_path).unwrap();
    let chunk = [b'x'; 64 * 1024];
    for _ in 0..ascii_bytes / chunk.len() {
        file.write_all(&chunk).unwrap();
    }
    file.write_all(&chunk[..ascii_bytes % chunk.len()]).unwrap();
    // Force the 1 MiB checkpoint to cross a multibyte scalar.
    let bridge = "🦀éΣ";
    file.seek(SeekFrom::Start(1024 * 1024 - 1)).unwrap();
    file.write_all(bridge.as_bytes()).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(tail.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let spec = Spec {
        targets: vec!["riscv64gc-unknown-none-elf".into()],
        help_source: Some(guide_path),
        ..Spec::new("guide-pack", "1.0.0")
    };
    let installed = package(app.install(&app.user, &spec));
    assert!(matches!(
        installed.availability,
        Some(PackageAvailability::Incompatible)
    ));
    drop(spec);
    settled(&app);
    let ((), largest, peak) = super::help_allocations::measure(|| regenerate(&app));
    eprintln!(
        "near-limit Help generation: guide={} largestAllocation={largest} peakAdditionalLive={peak}",
        ascii_bytes + tail.len()
    );
    assert!(
        largest < 4 * 1024 * 1024,
        "source-sized allocation: {largest}"
    );
    assert!(
        peak < 16 * 1024 * 1024,
        "source-sized live working set: {peak}"
    );
    let page = guides(&app, "1.0.0");
    let item = guide(&page).clone();
    let reference: devworks_core::protocol::provider::ArtifactRef =
        serde_json::from_value(item["content"].clone()).unwrap();
    let (old_generation, projection, old_reference) = read(&*app.store, |s| {
        let head = s.help().help_head()?;
        let projection = s.help().help_content(reference.sha256.as_str())?.unwrap();
        Ok((
            head.generation_id.unwrap(),
            projection,
            s.help()
                .help_content_reference(reference.sha256.as_str())?
                .unwrap(),
        ))
    });
    assert!(!projection.index_complete);
    assert!(projection.search_text.len() <= MAX_HELP_INDEX_BYTES);
    assert!(projection.offsets.len() <= MAX_HELP_CHECKPOINTS);
    assert_eq!(projection.byte_count, (ascii_bytes + tail.len()) as u64);
    assert_eq!(
        projection.character_count,
        (ascii_bytes - 5 + tail.chars().count()) as i64
    );
    let ((found, first), largest, peak) = super::help_allocations::measure(|| {
        let found = query(
            &app,
            "help.search",
            json!({"query":"full-tail-évidence","page":{"limit":100}}),
            3,
        );
        let first = query(
            &app,
            "help.read",
            json!({"topicId":item["id"],"version":"1.0.0","maxChars":"17"}),
            3,
        );
        (found, first)
    });
    assert!(
        found["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == item["id"])
    );
    assert_eq!(first["passage"]["text"], "x".repeat(17));
    assert!(
        largest < 4 * 1024 * 1024 && peak < 16 * 1024 * 1024,
        "search/read allocation {largest}, live {peak}"
    );
    // Position cursors confer no authority. Verify the binding against an actual issued cursor
    // before seeking the tail, avoiding thousands of redundant full-file integrity passes.
    let binding = read(&*app.store, |s| {
        let head = s.help().help_head()?;
        let operation = s
            .help()
            .help_generation_operation(head.generation_id.as_ref().unwrap())?
            .unwrap();
        let operation = s.operations().operation(&operation)?.unwrap();
        let principal = s
            .authority()
            .principal(app.user.principal())?
            .map_or(0, |p| p.revision);
        let basis = json!({"query":[item["id"],"1.0.0","17","3"],"generation":head.generation_id,"registry":s.packages().registry_revision()?,"source":operation.payload["inputDigest"],"principalRevision":principal}).to_string();
        Ok(CursorBinding::new(
            &app.user,
            "help.read",
            &[&basis],
            head.revision,
        ))
    });
    assert_eq!(first["passage"]["nextCursor"], binding.encode("17"));
    let ((tail_passage, unicode), largest, _) = super::help_allocations::measure(|| {
        let passage = query(
            &app,
            "help.read",
            json!({"topicId":item["id"],"version":"1.0.0","maxChars":"17","cursor":binding.encode(&(ascii_bytes - 5).to_string())}),
            3,
        );
        let unicode = query(
            &app,
            "help.search",
            json!({"query":"ος́! 終端🦀","page":{"limit":100}}),
            3,
        );
        (passage, unicode)
    });
    assert_eq!(
        tail_passage["passage"]["text"],
        tail.chars().take(17).collect::<String>()
    );
    assert!(
        unicode["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == item["id"])
    );
    assert!(largest < 4 * 1024 * 1024);
    let mut complete_tail = tail_passage["passage"]["text"].as_str().unwrap().to_owned();
    let mut cursor = tail_passage["passage"]["nextCursor"].clone();
    while cursor.is_string() {
        let page = query(
            &app,
            "help.read",
            json!({"topicId":item["id"],"version":"1.0.0","maxChars":"17","cursor":cursor}),
            3,
        );
        complete_tail.push_str(page["passage"]["text"].as_str().unwrap());
        cursor = page["passage"]["nextCursor"].clone();
    }
    assert_eq!(complete_tail, tail);
    assert_eq!(
        projection.offsets[1],
        HelpOffset {
            byte_offset: 1024 * 1024 + 3,
            character_offset: 1024 * 1024
        }
    );
    // Exercise both actual Gateway preparation controls while the native first hash is in
    // progress. The test clock changes only after several bounded file callbacks, so neither
    // case is a pre-admission refusal or a prefix-only successful result.
    struct StopClock {
        calls: AtomicUsize,
        cancel: Cancellation,
        now: i64,
        deadline: bool,
    }
    impl Clock for StopClock {
        fn now_ms(&self) -> devworks_core::protocol::Counter {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call >= 7 {
                if self.deadline {
                    return devworks_core::protocol::Counter::new(self.now + 1_000).unwrap();
                }
                self.cancel.cancel();
            }
            devworks_core::protocol::Counter::new(self.now).unwrap()
        }
    }
    for deadline in [false, true] {
        let cancellation = Cancellation::default();
        let clock = Arc::new(StopClock {
            calls: AtomicUsize::new(0),
            cancel: cancellation.clone(),
            now: app.h.now(),
            deadline,
        });
        let service = Arc::new(devworks_core::services::help::HelpService::new(
            app.store.clone(),
            app.app().artifacts().clone(),
            app.h.ids.clone(),
            clock.clone(),
            None,
        ));
        let cases = UseCases::new().with_help(service);
        let authority = HelpAuthority {
            store: app.store.clone(),
            clock: app.h.clock(),
        };
        let gateway = devworks_core::gateway::Gateway {
            authority: &authority,
            store: &*app.store,
            ids: &*app.h.ids,
        };
        let frame = json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":3,"queryId":app.key("controlled-read"),"body":{"kind":"help.read","topicId":item["id"],"version":"1.0.0","maxChars":"17"}});
        let before = read(&*app.store, |r| Ok(r.stamp().committed_sequence));
        let reply = cases
            .query_with(
                &gateway,
                &app.user,
                "help.read",
                frame.to_string().as_bytes(),
                ReadOptions {
                    cancellation: Some(cancellation),
                    deadline_ms: deadline.then_some(app.h.now() + 100),
                },
            )
            .unwrap();
        assert!(
            matches!(&reply,DomainQueryReply::Rejected{fault,..} if fault.code == DomainErrorCode::Cancelled),
            "{reply:?}"
        );
        assert!(clock.calls.load(Ordering::SeqCst) >= 8);
        assert_eq!(
            read(&*app.store, |r| Ok(r.stamp().committed_sequence)),
            before,
            "interrupted reads never publish anything"
        );
    }
    // An unrelated actual registry generation retains the one immutable full-body ref/index.
    let registry = app.registry();
    write(&*app.store, move |tx| {
        tx.packages().advance_registry(registry)
    })
    .unwrap();
    regenerate(&app);
    let next = guides(&app, "1.0.0");
    assert_eq!(guide(&next)["content"], item["content"]);
    read(&*app.store, |s| {
        assert_eq!(
            s.help().help_content(reference.sha256.as_str())?,
            Some(projection.clone())
        );
        assert_eq!(
            s.help()
                .help_topic(
                    &old_generation,
                    &Id::new(item["id"].as_str().unwrap()).unwrap()
                )?
                .unwrap()
                .content_ref,
            Some(old_reference)
        );
        Ok(())
    });
    let files = NativeArtifactFiles::new(app.h.root.clone(), app.h.root.with_extension("cache"));
    assert_eq!(
        files
            .stored_files()
            .unwrap()
            .iter()
            .filter(|(digest, _)| digest == reference.sha256.as_str())
            .count(),
        1
    );
    // A later installed source changes current Help, while the retained historical exact
    // artifact still exposes the original full Unicode tail to its actual Human audience.
    install(&app, "2.0.0");
    settled(&app);
    let mut exact = app
        .app()
        .artifacts()
        .open_exact_version(&*app.store, &app.user, &reference)
        .unwrap();
    std::io::copy(
        &mut exact.reader.by_ref().take(ascii_bytes as u64),
        &mut std::io::sink(),
    )
    .unwrap();
    let mut historical_tail = String::new();
    exact.reader.read_to_string(&mut historical_tail).unwrap();
    assert_eq!(historical_tail, tail);
    // Confirm the actual installed source remains a regular file; no generated prefix replaced it.
    let installed_path = app
        .h
        .root
        .join("packages/guide-pack")
        .join(String::from(installed.digest))
        .join("help.md");
    let mut source = std::fs::File::open(installed_path).unwrap();
    source.seek(SeekFrom::Start(ascii_bytes as u64)).unwrap();
    let mut actual_tail = String::new();
    source.read_to_string(&mut actual_tail).unwrap();
    assert_eq!(actual_tail, tail);
}
