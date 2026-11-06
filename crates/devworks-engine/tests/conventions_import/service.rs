//! Native shared-owner regressions: no test handler, direct catalog inserts or model execution.
#[path = "../../src/conventions/indexing.rs"]
mod actual_index_consumer;
use super::*;
fn imported_order(f: &Fx, name: &str, order: Option<i32>) -> StoredConvention {
    let mut value = manifest(0);
    value["slug"] = json!(name);
    value["title"] = json!(name);
    match order {
        Some(order) => value["sortOrder"] = json!(order),
        None => {
            value.as_object_mut().unwrap().remove("sortOrder");
        }
    }
    let source = f.source(&serde_json::to_vec(&value).unwrap());
    let op = f.admit(&source, &format!("create-{name}"));
    let result = f.dispatch(&op);
    assert_eq!(result.state, OperationState::ObservedComplete, "{result:?}");
    let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    read(f.store.as_ref(), |r| {
        r.conventions()
            .convention(&draft.convention_id, None, &[KnowledgeScope::Workspace])
    })
    .unwrap()
}
#[test]
fn omitted_order_appends_but_explicit_zero_remains_an_explicit_choice() {
    let f = Fx::new();
    assert_eq!(imported_order(&f, "first", None).sort_order, 0);
    assert_eq!(imported_order(&f, "append", None).sort_order, 1);
    assert_eq!(imported_order(&f, "explicit-zero", Some(0)).sort_order, 0);
    assert_eq!(
        imported_order(&f, "append-after-explicit", None).sort_order,
        2
    );
}
#[test]
fn omitted_order_retains_frozen_unchecked_i32_maximum_wrapping() {
    let f = Fx::new();
    assert_eq!(
        imported_order(&f, "maximum", Some(i32::MAX)).sort_order,
        i64::from(i32::MAX)
    );
    assert_eq!(
        imported_order(&f, "wrapped", None).sort_order,
        i64::from(i32::MIN)
    );
    assert_eq!(imported_order(&f, "still-explicit", Some(0)).sort_order, 0);
}

fn imported_literal(
    f: &Fx,
    slug: &str,
    title: &str,
    parent: &[&str],
    key: &str,
) -> StoredConvention {
    let mut value = manifest(0);
    value["slug"] = json!(slug);
    value["title"] = json!(title);
    value["parentPath"] = json!(parent);
    let source = f.source(&serde_json::to_vec(&value).unwrap());
    let op = f.admit(&source, key);
    assert_eq!(f.dispatch(&op).state, OperationState::ObservedComplete);
    let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    read(f.store.as_ref(), |r| {
        r.conventions().convention(
            &draft.convention_id,
            None,
            &[f.ctx
                .project_id()
                .cloned()
                .map_or(KnowledgeScope::Workspace, KnowledgeScope::Project)],
        )
    })
    .unwrap()
}
fn query_with(f: &Fx, body: serde_json::Value, options: ReadOptions) -> serde_json::Value {
    let authority = HostAuthority {
        store: &*f.store,
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*f.store,
        authority: &authority,
        ids: &*f.h.ids,
    };
    let operation = body["kind"].as_str().unwrap();
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"actual-source-read","body":body});
    serde_json::to_value(
        UseCases::new()
            .with_conventions(f.catalog.clone())
            .query_with(
                &gateway,
                &f.ctx,
                operation,
                frame.to_string().as_bytes(),
                options,
            )
            .unwrap(),
    )
    .unwrap()
}
fn query(f: &Fx, body: serde_json::Value) -> serde_json::Value {
    query_with(f, body, ReadOptions::default())
}
fn read_id(value: &serde_json::Value) -> &serde_json::Value {
    &value["result"]["value"]["view"]["convention"]["id"]
}
#[test]
fn exact_hierarchy_and_root_slug_fuzzy_tool_lookup_do_not_alias_each_other() {
    let f = Fx::new();
    imported_literal(&f, "parent", "Parent", &[], "hierarchy-parent");
    let child = imported_literal(&f, "child", "Child", &["parent"], "hierarchy-child");
    let fuzzy = imported_literal(
        &f,
        "fuzzy",
        "literal parent/child match",
        &[],
        "fuzzy-literal",
    );
    let slug = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"slug":"parent/child"}),
    );
    assert_eq!(read_id(&slug), &json!(fuzzy.id), "{slug}");
    let exact = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":["parent","child"]}),
    );
    assert_eq!(read_id(&exact), &json!(child.id), "{exact}");
    let document = query(
        &f,
        json!({"kind":"conventions.resources","scope":{"kind":"Workspace"},"applicability":[],"page":{"limit":10},"selection":{"kind":"Document","rootSlug":"parent/child"}}),
    );
    assert_eq!(document["fault"]["code"], "NotFound", "{document}");
    let literal = imported_literal(&f, "parent/child", "Literal root", &[], "literal-root");
    let root = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"slug":"parent/child"}),
    );
    assert_eq!(read_id(&root), &json!(literal.id), "{root}");
    let exact = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":["parent","child"]}),
    );
    assert_eq!(read_id(&exact), &json!(child.id), "{exact}");
    let absent = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":["parent/child","missing"]}),
    );
    assert_eq!(absent["fault"]["code"], "NotFound", "{absent}");
}
#[test]
fn exact_path_preserves_empty_nul_and_separator_components_without_normalizing() {
    let f = Fx::new();
    imported_literal(&f, "", "Empty root", &[], "empty-parent");
    let child = imported_literal(&f, "a/b\0", "Literal child", &[""], "literal-child");
    let exact = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":["","a/b\0"]}),
    );
    assert_eq!(read_id(&exact), &json!(child.id), "{exact}");
    let split = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":["","a","b\0"]}),
    );
    assert_eq!(split["fault"]["code"], "NotFound", "{split}");
    let empty = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":[""]}),
    );
    assert_eq!(
        empty["result"]["value"]["view"]["convention"]["slug"], "",
        "{empty}"
    );
}
#[test]
fn out_of_range_or_mid_scalar_path_cursors_are_refused_before_literal_indexing() {
    let f = Fx::new();
    let owner = imported_literal(&f, "unicode-root", "λ Root", &[], "path-owner");
    let dispatcher = dispatcher_with(
        &f.h,
        f.store.clone(),
        vec![actual_index_consumer::consumer(
            f.store.clone(),
            f.catalog.indexing(),
        )],
        |_| {},
    );
    runtime().block_on(async {
        dispatcher.recover().await.unwrap();
        dispatcher.run_until_idle().await.unwrap();
    });
    let (head, chunk, revision) = read(&*f.store, |r| {
        let head = r.conventions().convention_index_head(&owner.id)?.unwrap();
        let chunk = r
            .conventions()
            .convention_chunks(&head.generation_id, None, 1)?
            .remove(0);
        let revision = r
            .conventions()
            .convention_scope_revision(&KnowledgeScope::Workspace)?;
        Ok((head, chunk, revision))
    });
    let body = json!({"kind":"conventions.read","conventionId":owner.id,"format":"SectionPath","sectionPath":{"generationId":head.generation_id,"chunkId":chunk.id},"maxChars":"1"});
    let valid = query(&f, body.clone());
    assert_eq!(
        valid["result"]["value"]["view"]["passage"]["text"], "λ",
        "{valid}"
    );
    let mut input = body.clone();
    input.as_object_mut().unwrap().remove("kind");
    let intent = serde_json::to_string(&input).unwrap();
    let binding = devworks_core::services::cursor::CursorBinding::new(
        &f.ctx,
        "conventions.path",
        &[&intent, head.generation_id.as_str(), chunk.id.as_str()],
        revision.max(1),
    );
    for position in ["0:0:1", "0:0:18446744073709551615", "0:999999:0"] {
        let mut bad = body.clone();
        bad["cursor"] = json!(binding.encode(position));
        let refused = query(&f, bad);
        assert_eq!(
            refused["fault"]["code"], "CursorExpired",
            "{position}: {refused}"
        );
    }
}
#[test]
fn cancellation_inside_the_real_body_read_remains_cancelled_in_the_typed_reply() {
    let h = Harness::new();
    let (started, waiting) = std::sync::mpsc::sync_channel(1);
    let files = Arc::new(GateFiles {
        native: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("import-cache")),
        calls: std::sync::atomic::AtomicUsize::new(0),
        started,
        release: std::sync::Mutex::new(true),
        changed: std::sync::Condvar::new(),
    });
    let _release = ReleaseGate(files.clone());
    let f = Fx::with_files(h, files.clone());
    let owner = imported_literal(&f, "cancelled-read", "Cancel read", &[], "read-owner");
    while waiting.try_recv().is_ok() {}
    *files.release.lock().unwrap() = false;
    files.calls.store(2, std::sync::atomic::Ordering::SeqCst);
    let cancellation = Cancellation::new();
    let child_cancel = cancellation.clone();
    let thread = std::thread::spawn(move || {
        query_with(
            &f,
            json!({"kind":"conventions.read","conventionId":owner.id}),
            ReadOptions {
                cancellation: Some(child_cancel),
                deadline_ms: None,
            },
        )
    });
    waiting
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("query did not enter real controlled file read");
    cancellation.cancel();
    *files.release.lock().unwrap() = true;
    files.changed.notify_all();
    let reply = thread.join().unwrap();
    assert_eq!(reply["fault"]["code"], "Cancelled", "{reply}");
}

#[test]
fn project_paths_keep_inherited_general_guidance_but_refuse_ambiguous_roots() {
    let mut f = Fx::new();
    let general = imported_literal(&f, "shared", "General", &[], "general-guidance");
    let project = devworks_core::protocol::ProjectId(id("path-project"));
    let p = project.clone();
    write(&*f.store, move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: p,
            name: "Path project".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })
    })
    .unwrap();
    f.ctx = f.ctx.with_lineage(Some(project.clone()), None, None);
    let inherited = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Project","projectId":project},"path":["shared"]}),
    );
    assert_eq!(read_id(&inherited), &json!(general.id), "{inherited}");
    imported_literal(&f, "shared", "Project-local", &[], "project-guidance");
    let ambiguous = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Project","projectId":project},"path":["shared"]}),
    );
    assert_eq!(ambiguous["fault"]["code"], "InvalidArgument", "{ambiguous}");
    assert_eq!(
        ambiguous["fault"]["targetKind"], "ambiguous_convention",
        "{ambiguous}"
    );
    let workspace = query(
        &f,
        json!({"kind":"conventions.read","scope":{"kind":"Workspace"},"path":["shared"]}),
    );
    assert_eq!(read_id(&workspace), &json!(general.id), "{workspace}");
}
