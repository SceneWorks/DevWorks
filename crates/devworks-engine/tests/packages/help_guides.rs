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
