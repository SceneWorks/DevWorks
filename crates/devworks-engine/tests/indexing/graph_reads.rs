//! Real registered read operators, retained source pages and exact trust mutations.
use super::*;
use devworks_core::protocol::data::{GraphPage, GraphResult};
fn page(
    app: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    query: Value,
    view: Option<Value>,
    cursor: Option<&str>,
) -> GraphPage {
    let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"reads-native","scope":{"kind":"Workspace"},"query":query,"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    if let Some(view) = view {
        frame["view"] = view;
    } else {
        frame["current"] = json!(true);
    }
    if let Some(cursor) = cursor {
        frame["cursor"] = json!(cursor);
    }
    let reply = app
        .query(ctx, operation, frame.to_string().as_bytes())
        .unwrap();
    let value = match reply {
        DomainQueryReply::Result { result, .. } => serde_json::to_value(result).unwrap(),
        other => panic!("{operation}: {other:?}"),
    };
    serde_json::from_value(value["value"].clone()).unwrap()
}
#[test]
fn native_graph_reads_reuse_one_pin_and_stream_exact_source_after_checkout_removal() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("reads");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn target() {}\npub fn caller(){target();}\npub fn outer(){caller();}\n",
    )
    .unwrap();
    let large = format!("{}\0end\n", "Unicode 世界\n".repeat(10000));
    std::fs::write(repo.join("README"), &large).unwrap();
    std::fs::write(repo.join("empty.txt"), "").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "read operators"]);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let run = index(&app, &ctx, &repositories[0], "reads-index");
    executor.block_on(within("read source publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let unified = page(
        &app,
        &ctx,
        "code.search",
        json!({"kind":"Search","selection":{"mode":"Unified","text":"reads"}}),
        None,
        None,
    );
    let GraphResult::CodeSearch { value: unified } = unified.result else {
        panic!("unified repository search")
    };
    let repository = unified
        .items
        .0
        .iter()
        .find_map(|hit| match hit {
            devworks_core::protocol::records::CodeSearchHit::Repository { repository }
                if repository.repository_id.as_str() == repositories[0].as_str() =>
            {
                Some(repository)
            }
            _ => None,
        })
        .expect("actual registered repository hit");
    assert!(
        repository.foundational.is_none(),
        "unrecorded classification stays unknown"
    );
    let found = page(
        &app,
        &ctx,
        "code.search",
        json!({"kind":"Search","selection":{"mode":"NodeList","text":"target","repositoryId":repositories[0]}}),
        None,
        None,
    );
    let GraphResult::CodeSearch { value } = &found.result else {
        panic!("{found:?}")
    };
    let target = value
        .items
        .0
        .iter()
        .find_map(|hit| match hit {
            devworks_core::protocol::records::CodeSearchHit::Node { node }
                if node.name.0 == "target" =>
            {
                Some(node.clone())
            }
            _ => None,
        })
        .unwrap();
    let pinned = json!({"kind":"Pinned","pinId":found.view.as_ref().unwrap().pin_id});
    std::fs::remove_dir_all(&repo).unwrap();
    let detail = page(
        &app,
        &ctx,
        "code.node_detail",
        json!({"kind":"Node","nodeId":target.node_id}),
        Some(pinned.clone()),
        None,
    );
    let GraphResult::NodeDetail { value } = detail.result else {
        panic!("detail")
    };
    assert_eq!(
        value
            .incoming
            .0
            .iter()
            .filter(|e| e.edge_kind.0 == "CALLS")
            .count(),
        1
    );
    for (operation, query) in [
        (
            "code.nodes_by_file",
            json!({"kind":"NodesByFile","selection":{"repositoryId":repositories[0],"path":"src/lib.rs","line":"1"}}),
        ),
        (
            "code.call_path",
            json!({"kind":"Path","selection":{"target":{"kind":"Node","nodeId":target.node_id},"direction":"Inbound","depth":3}}),
        ),
        (
            "code.traverse",
            json!({"kind":"Traverse","startIds":[target.node_id],"direction":"Reverse","edgeKinds":["CALLS"],"maxDepth":3}),
        ),
        (
            "code.dependency",
            json!({"kind":"Traverse","startIds":[target.node_id],"direction":"Reverse","edgeKinds":["CALLS"],"maxDepth":3}),
        ),
        (
            "code.consumers",
            json!({"kind":"Consumers","selection":{"kind":"Node","nodeId":target.node_id}}),
        ),
        (
            "code.publishers",
            json!({"kind":"Publishers","selection":{"kind":"Node","nodeId":target.node_id}}),
        ),
        (
            "code.data_lineage",
            json!({"kind":"DataLineage","selection":{"kind":"Node","nodeId":target.node_id}}),
        ),
        ("code.graph_scope_status", json!({"kind":"ScopeStatus"})),
    ] {
        let answer = page(&app, &ctx, operation, query, Some(pinned.clone()), None);
        assert_eq!(
            answer.view.as_ref().unwrap().pin_id,
            found.view.as_ref().unwrap().pin_id,
            "{operation}"
        );
    }
    let query = json!({"kind":"Source","selection":{"kind":"File","repositoryId":repositories[0],"path":"README","startLine":"0","endLine":"0"}});
    let mut cursor = None;
    let mut bytes = Vec::<u8>::new();
    let mut pages = 0;
    loop {
        let answer = page(
            &app,
            &ctx,
            "code.source",
            query.clone(),
            Some(pinned.clone()),
            cursor.as_deref(),
        );
        let GraphResult::SourceText { value } = answer.result else {
            panic!("source")
        };
        assert_eq!(value.start_byte.get() as usize, bytes.len());
        assert!(value.text.0.len() <= 65536);
        bytes.extend_from_slice(value.text.0.as_bytes());
        pages += 1;
        cursor = answer.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(pages > 1);
    assert_eq!(bytes, large.as_bytes());
    let empty = page(
        &app,
        &ctx,
        "code.snippet",
        json!({"kind":"Source","selection":{"kind":"File","repositoryId":repositories[0],"path":"empty.txt","startLine":"0","endLine":"0"}}),
        Some(pinned.clone()),
        None,
    );
    let GraphResult::SourceText { value } = empty.result else {
        panic!("empty")
    };
    assert_eq!(value.total_bytes.get(), 0);
    assert_eq!(value.first_line.get(), 0);
    let file_id = read(&*store, |snapshot| {
        let view = snapshot.indexing().view(&target.view_id)?.unwrap();
        let scope = GraphScope {
            views: vec![GraphViewBinding {
                repository_id: target.repository_id.clone(),
                view_id: view.view_id,
                view_digest: view.view_digest,
            }],
        };
        let page = snapshot.graph().graph_nodes(
            &scope,
            &GraphNodeFilter {
                unit_key: Some("README".into()),
                kinds: vec!["File".into()],
                ..Default::default()
            },
            None,
            2,
        )?;
        Ok(id(&page.items[0].node.node_id))
    });
    let legacy = json!({"protocol":"1.0-draft.1","schemaRevision":1,"queryId":"legacy-source","scope":{"kind":"Workspace"},"view":pinned,"query":{"kind":"Source","nodeId":file_id,"maxLines":1000},"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let old = app
        .query(&ctx, "code.source", legacy.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::CodeSource { value: old },
        ..
    } = old
    else {
        panic!("{old:?}")
    };
    let GraphResult::Source { citation, text } = old.result else {
        panic!("legacy source")
    };
    assert_eq!(text.lines().count(), 1000);
    assert!(matches!(
        citation.locator,
        devworks_core::protocol::data::Locator::Code {
            first_line: 1,
            last_line: 1000,
            ..
        }
    ));
    let listed = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Query","queryId":"listed","body":{"kind":"repositories.nodes","repositoryId":target.repository_id,"scope":{"kind":"Workspace"},"views":pinned,"page":{"limit":100}}});
    let listed = app
        .query(&ctx, "repositories.nodes", listed.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(
            &listed,
            DomainQueryReply::Result {
                result: OperationResult::RepositoriesNodes { .. },
                ..
            }
        ),
        "{listed:?}"
    );
    let command = json!({"protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"trust-native","command":{"kind":"SetTrust","repositoryId":target.repository_id,"nodeId":target.node_id,"expectedRevision":target.trust_revision,"viewId":target.view_id,"trusted":false,"unitKey":target.unit_key,"expectedViewDigest":target.view_digest,"expectedContentIdentity":target.content_identity,"scope":{"kind":"Workspace"},"pinId":found.view.unwrap().pin_id}});
    let reply = app
        .command(&ctx, "code.set_trust", command.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&reply,CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Applied{..})),
        "{reply:?}"
    );
    let after = page(
        &app,
        &ctx,
        "code.node_detail",
        json!({"kind":"Node","nodeId":target.node_id}),
        Some(pinned),
        None,
    );
    let GraphResult::NodeDetail { value } = after.result else {
        panic!("trust detail")
    };
    assert!(value.node.do_not_trust);
    assert_eq!(value.node.trust_revision.get(), 2);
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_graph_pin_keeps_selection_gaps_and_rechecks_authority_for_every_member() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    for name in ["indexed", "missing", "added"] {
        let repo = src.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("README"), format!("{name}\n")).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "pin member"]);
    }
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let user = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &user, &src);
    let registered = discovered(&app, &user);
    let mut by_name = std::collections::BTreeMap::new();
    for repository in registered {
        let row = read(&*store, |read| read.projects().repository(&id(&repository))).unwrap();
        by_name.insert(row.name, repository);
    }
    let run = index(&app, &user, &by_name["indexed"], "pin-first");
    executor.block_on(within("first pin member publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let members: Vec<_> = by_name
        .iter()
        .filter(|(name, _)| name.as_str() != "added")
        .map(|(_, s)| id(s))
        .collect();
    write(&*store, move |tx| {
        let now = tx.now_ms();
        let project = StoredProject {
            id: devworks_core::protocol::ProjectId(id("graph-project")),
            name: "Graph".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 1,
            archive_stop_pending: false,
        };
        tx.projects().insert_project(&project)?;
        tx.projects().set_members(&project.id, &members)
    })
    .unwrap();
    let ctx = user.clone().with_lineage(
        Some(devworks_core::protocol::ProjectId(id("graph-project"))),
        None,
        None,
    );
    let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"pin-members","scope":{"kind":"Project","projectId":"graph-project"},"current":true,"query":{"kind":"Search","selection":{"mode":"NodeList","text":"%"}},"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let query = |frame: &Value| {
        app.query(&ctx, "code.search", frame.to_string().as_bytes())
            .unwrap()
    };
    let first = match query(&frame) {
        DomainQueryReply::Result { result, .. } => serde_json::to_value(result).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        first["value"]["scopeCoverage"]["coverage"]["state"],
        "Partial"
    );
    assert_eq!(first["value"]["view"]["views"].as_array().unwrap().len(), 1);
    let pin = first["value"]["view"]["pinId"].clone();
    // Broad authority may grow after selection; the retained selection itself must not.
    let expanded: Vec<_> = by_name.values().map(|value| id(value)).collect();
    write(&*store, move |tx| {
        let key = devworks_core::protocol::ProjectId(id("graph-project"));
        let mut project = tx.projects().project(&key)?.unwrap();
        let expected = project.revision;
        project.revision += 1;
        project.scope_revision += 1;
        tx.projects().update_project(&project, expected)?;
        tx.projects().set_members(&key, &expanded)
    })
    .unwrap();
    let mut overview = frame.clone();
    overview.as_object_mut().unwrap().remove("current");
    overview["view"] = json!({"kind":"Pinned","pinId":pin});
    overview["query"] = json!({"kind":"Overview"});
    let overview = app
        .query(&ctx, "code.overview", overview.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::CodeOverview { value },
        ..
    } = overview
    else {
        panic!("{overview:?}")
    };
    let GraphResult::RepositoryOverview { value } = value.result else {
        panic!("overview")
    };
    assert_eq!(
        value.repositories.0.len(),
        2,
        "new project members cannot widen the retained selection"
    );
    let mut status = frame.clone();
    status.as_object_mut().unwrap().remove("current");
    status["view"] = json!({"kind":"Pinned","pinId":pin});
    status["query"] = json!({"kind":"ScopeStatus"});
    status["limits"]["maxResults"] = json!(1);
    let mut status_ids = std::collections::BTreeSet::new();
    let mut status_pages = 0;
    loop {
        let reply = app
            .query(
                &ctx,
                "code.graph_scope_status",
                status.to_string().as_bytes(),
            )
            .unwrap();
        let DomainQueryReply::Result {
            result: OperationResult::CodeGraphScopeStatus { value },
            ..
        } = reply
        else {
            panic!("{reply:?}")
        };
        let GraphResult::ScopeDetails { value: scope } = value.result else {
            panic!("scope")
        };
        for row in scope.repositories.0 {
            assert!(
                status_ids.insert(row.repository_id.as_str().to_owned()),
                "status page repeated"
            );
        }
        status_pages += 1;
        assert!(status_pages <= 2);
        if let Some(cursor) = value.next_cursor {
            status["cursor"] = json!(cursor);
        } else {
            break;
        }
    }
    assert_eq!(status_ids.len(), 2);

    let run = index(&app, &user, &by_name["missing"], "pin-second");
    executor.block_on(within("second pin member publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    frame.as_object_mut().unwrap().remove("current");
    frame["view"] = json!({"kind":"Pinned","pinId":pin});
    let retained = match query(&frame) {
        DomainQueryReply::Result { result, .. } => serde_json::to_value(result).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        retained["value"]["view"]["views"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        retained["value"]["scopeCoverage"]["unavailableRepositories"][0]["repositoryId"],
        by_name["missing"]
    );
    let remaining = id(&by_name["indexed"]);
    write(&*store, move |tx| {
        let key = devworks_core::protocol::ProjectId(id("graph-project"));
        let mut project = tx.projects().project(&key)?.unwrap();
        let expected = project.revision;
        project.revision += 1;
        project.scope_revision += 1;
        tx.projects().update_project(&project, expected)?;
        tx.projects().set_members(&key, &[remaining])
    })
    .unwrap();
    assert!(
        matches!(query(&frame), DomainQueryReply::Rejected { .. }),
        "removing even the frozen missing member must invalidate authority for the whole pin"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

/// A deterministic interruption trigger around the real native verified file reader. It does
/// not provide bytes, alter hashes, or replace the filesystem/storage/backend.
struct ControlledFiles {
    inner: NativeArtifactFiles,
    mode: std::sync::atomic::AtomicU8,
    cancel: Cancellation,
    checks: std::sync::atomic::AtomicUsize,
}
macro_rules! forward_files {($($name:ident($($arg:ident:$ty:ty),*)->$out:ty;)*)=>{$(fn $name(&self,$($arg:$ty),*)->Result<$out,devworks_core::ports::files::FileFault>{devworks_core::ports::files::ArtifactFiles::$name(&self.inner,$($arg),*)})*};}
impl devworks_core::ports::files::ArtifactFiles for ControlledFiles {
    forward_files! {
     publish(staging_name:&str,source:&mut dyn std::io::Read,digest:&str,bytes:u64,reuse:Option<&str>,generation:&str)->devworks_core::ports::files::PublishedBlob;
     managed_path(digest:&str,generation:&str)->String;
     present(digest:&str,generation:&str,bytes:u64)->bool;
     inspect(digest:&str,generation:&str)->Option<u64>;
     read_range(digest:&str,generation:&str,bytes:u64,offset:u64,max:usize)->Vec<u8>;
     open(digest:&str,generation:&str,bytes:u64)->Box<dyn std::io::Read+Send>;
     unlink(digest:&str,generation:&str)->();
     quarantine(digest:&str,generation:&str)->();
     staged()->Vec<String>;
     remove_staged(name:&str)->();
     stored_files()->Vec<(String,String)>;
     write_derived(key:&str,bytes:&[u8])->();
     read_derived(key:&str,max:usize)->Option<Vec<u8>>;
     collect_derived()->u64;
     export(digest:&str,generation:&str,bytes:u64,destination:&Path,overwrite:bool)->();
    }
    fn open_controlled(
        &self,
        digest: &str,
        generation: &str,
        bytes: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn std::io::Read + Send>, devworks_core::ports::files::FileFault> {
        self.inner
            .open_controlled(digest, generation, bytes, &mut || {
                let check = self
                    .checks
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match self.mode.load(std::sync::atomic::Ordering::SeqCst) {
                    1 if check == 2 => self.cancel.cancel(),
                    2 if check == 2 => std::thread::sleep(Duration::from_millis(1100)),
                    _ => {}
                }
                stopped()
            })
    }
}
#[test]
fn native_source_verification_obeys_cancel_deadline_and_work_bounds_without_poisoning_reads() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("bounded-source");
    std::fs::create_dir_all(&repo).unwrap();
    let source = "actual native verified bytes\n".repeat(10000);
    std::fs::write(repo.join("README"), &source).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "bounded source"]);
    let cancellation = Cancellation::new();
    let files = Arc::new(ControlledFiles {
        inner: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache")),
        mode: std::sync::atomic::AtomicU8::new(0),
        cancel: cancellation.clone(),
        checks: std::sync::atomic::AtomicUsize::new(0),
    });
    let artifacts = Arc::new(ArtifactService::new(
        files.clone(),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut composed = composition(&h, store.clone(), "git");
    composed.artifacts = artifacts.clone();
    let app = executor.block_on(boot(composed)).unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repository = discovered(&app, &ctx).remove(0);
    let run = index(&app, &ctx, &repository, "bounded-source-index");
    executor.block_on(within("bounded source publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let service = devworks_core::services::graph::GraphService::new(
        store.clone(),
        h.clock(),
        h.ids.clone(),
        artifacts,
    );
    let gateway = devworks_core::gateway::Gateway {
        authority: &Authority,
        store: &*store,
        ids: &*h.ids,
    };
    let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"bounded-source","scope":{"kind":"Workspace"},"view":{"kind":"Published","repositoryIds":[repository]},"query":{"kind":"Source","selection":{"kind":"File","repositoryId":repository,"path":"README","startLine":"0","endLine":"0"}},"limits":{"maxResults":100,"maxVisited":2,"deadlineMs":30000}});
    let work = gateway
        .query(&ctx, "code.source", frame.to_string().as_bytes(), &service)
        .unwrap();
    assert!(
        matches!(&work,DomainQueryReply::Rejected{fault,..}if fault.target_kind.as_ref().is_some_and(|kind|kind.0=="graph_visited_limit")),
        "{work:?}"
    );
    frame["limits"]["maxVisited"] = json!(100000);
    files.checks.store(0, std::sync::atomic::Ordering::SeqCst);
    files.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    let cancelled = gateway
        .query_with(
            &ctx,
            "code.source",
            frame.to_string().as_bytes(),
            &service,
            ReadOptions {
                cancellation: Some(cancellation),
                deadline_ms: None,
            },
        )
        .unwrap();
    assert!(
        matches!(&cancelled,DomainQueryReply::Rejected{fault,..}if fault.code==devworks_core::protocol::DomainErrorCode::Cancelled),
        "{cancelled:?}"
    );
    assert!(files.checks.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    files.checks.store(0, std::sync::atomic::Ordering::SeqCst);
    files.mode.store(2, std::sync::atomic::Ordering::SeqCst);
    frame["limits"]["deadlineMs"] = json!(1000);
    let deadline = gateway
        .query(&ctx, "code.source", frame.to_string().as_bytes(), &service)
        .unwrap();
    assert!(
        matches!(&deadline,DomainQueryReply::Rejected{fault,..}if fault.target_kind.as_ref().is_some_and(|kind|kind.0=="graph_deadline")),
        "{deadline:?}"
    );
    assert!(files.checks.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    files.mode.store(0, std::sync::atomic::Ordering::SeqCst);
    frame["limits"]["deadlineMs"] = json!(30000);
    let recovered = gateway
        .query(&ctx, "code.source", frame.to_string().as_bytes(), &service)
        .unwrap();
    assert!(
        matches!(recovered, DomainQueryReply::Result { .. }),
        "{recovered:?}"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_swift_dependency_pin_does_not_duplicate_its_declared_cross_repository_link() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    for (name, manifest) in [
        (
            "swift-core",
            "// swift-tools-version: 5.9\nimport PackageDescription\nlet package = Package(name: \"swift-core\")\n",
        ),
        (
            "swift-consumer",
            "// swift-tools-version: 5.9\nimport PackageDescription\nlet package = Package(name: \"swift-consumer\", dependencies: [.package(url: \"https://example.invalid/swift-core.git\", from: \"1.0.0\")])\n",
        ),
    ] {
        let repo = src.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("Package.swift"), manifest).unwrap();
        if name == "swift-consumer" {
            std::fs::write(repo.join("Package.resolved"),json!({"version":2,"pins":[{"identity":"swift-core","kind":"remoteSourceControl","location":"https://example.invalid/swift-core.git","state":{"revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","version":"1.0.0"}}]}).to_string()).unwrap();
        }
        git(&repo, &["init", "-q"]);
        if name == "swift-core" {
            git(
                &repo,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/swift-core.git",
                ],
            );
        }
        git(&repo, &["add", "-A"]);
        git(
            &repo,
            &["commit", "-q", "-m", "Swift captured dependencies"],
        );
    }
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let mut consumer = None;
    for repository in &repositories {
        let record = read(&*store, |s| s.projects().repository(&id(repository))).unwrap();
        if record.name == "swift-consumer" {
            consumer = Some(repository.clone());
        }
        let run = index(&app, &ctx, repository, &format!("swift-index-{repository}"));
        executor.block_on(within("Swift manifests publish", async {
            loop {
                let state = run_state(&store, &run);
                if state.state == RUN_SUCCEEDED {
                    break;
                }
                assert!(!run_terminal(&state.state), "{state:?}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }));
    }
    let answer = page(
        &app,
        &ctx,
        "code.repository_dependencies",
        json!({"kind":"RepositoryDependencies","repositoryId":consumer.unwrap()}),
        Some(json!({"kind":"Published","repositoryIds":repositories})),
        None,
    );
    let GraphResult::Dependencies { value } = answer.result else {
        panic!("dependencies")
    };
    let count: u64 = value
        .outbound
        .0
        .iter()
        .filter(|link| link.edge_kind.0 == "REFERENCES_PACKAGE")
        .map(|link| link.count.get() as u64)
        .sum();
    assert_eq!(
        count, 1,
        "a resolved pin repeats one declared logical dependency"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_current_maps_owned_git_identity_to_the_actual_indexed_worktree_without_main_fallback() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let main = src.join("main");
    let owned = src.join("owned");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(main.join("README"), "published main\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-q", "-m", "baseline"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "owned",
            owned.to_str().unwrap(),
        ],
    );
    std::fs::write(owned.join("README"), "owned dirty capture\n").unwrap();
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let user = principal(&*store, "user", PrincipalKind::Human);
    let agent = principal(&*store, "agent", PrincipalKind::Agent);
    base_directory(&store, &user, &src);
    let repositories = discovered(&app, &user);
    assert_eq!(repositories.len(), 1);
    let repository = id(&repositories[0]);
    let run = index(&app, &user, &repositories[0], "owned-baseline");
    executor.block_on(within("owned baseline publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let tree = read(&*store, |snapshot| {
        let rows = snapshot.indexing().worktrees(&repository)?;
        Ok(rows.into_iter().find(|tree| !tree.main).unwrap())
    });
    assert_ne!(tree.worktree_id, id("git-owned-record"));
    let actor = agent.principal().clone();
    let path = tree.path.clone();
    let head = tree.head_oid.clone().unwrap();
    let repo_id = repository.clone();
    write(&*store, move |tx| {
        let now = tx.now_ms();
        let project = StoredProject {
            id: devworks_core::protocol::ProjectId(id("owned-project")),
            name: "Owned".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 1,
            archive_stop_pending: false,
        };
        tx.projects().insert_project(&project)?;
        tx.projects()
            .set_members(&project.id, std::slice::from_ref(&repo_id))?;
        let work = StoredWork {
            id: id("owned-work"),
            project_id: project.id.clone(),
            kind: TASK.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Owned".into(),
            priority: 0,
            manual_order: 1,
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            created_by: Some(actor.clone()),
            updated_by: Some(actor.clone()),
            search_key: "owned".into(),
            caller_ref: None,
            completion: None,
            brief: devworks_core::services::work::model::WorkBrief::default().value(),
        };
        tx.work().insert_work(&work)?;
        tx.work()
            .set_work_repositories(&work.id, std::slice::from_ref(&repo_id))?;
        tx.git().insert_git_worktree(&StoredGitWorktree {
            id: id("git-owned-record"),
            repository_id: repo_id,
            work_id: work.id,
            owner: actor,
            ownership_revision: 1,
            owner_root_id: Some(devworks_core::protocol::RootId(id("owned-work"))),
            owner_assignment_id: None,
            registered_root_id: id("fixture-native-root"),
            path,
            branch: "refs/heads/owned".into(),
            head: serde_json::from_value(json!({"format":"Sha1","hex":head})).unwrap(),
            revision: 1,
            removed: false,
            location: None,
            pending_operation_id: None,
            last_operation_id: None,
            last_result: None,
        })
    })
    .unwrap();
    let context = devworks_core::services::git::context::bind_worktree(
        &*store,
        agent.with_lineage(
            Some(devworks_core::protocol::ProjectId(id("owned-project"))),
            Some(devworks_core::protocol::RootId(id("owned-work"))),
            None,
        ),
        &id("git-owned-record"),
    )
    .unwrap();
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"owned-current","scope":{"kind":"Project","projectId":"owned-project"},"current":true,"query":{"kind":"Source","selection":{"kind":"File","repositoryId":repository,"path":"README","startLine":"0","endLine":"0"}},"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    assert!(
        matches!(app.query(&context,"code.source",frame.to_string().as_bytes()).unwrap(),DomainQueryReply::Rejected{fault,..}if fault.code==devworks_core::protocol::DomainErrorCode::ViewUnavailable),
        "an unprepared owned view must never fall back to main"
    );
    let mut status = frame.clone();
    status["query"] = json!({"kind":"ScopeStatus"});
    let unavailable = app
        .query(
            &context,
            "code.graph_scope_status",
            status.to_string().as_bytes(),
        )
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::CodeGraphScopeStatus { value },
        ..
    } = unavailable
    else {
        panic!("{unavailable:?}")
    };
    assert!(value.view.is_none());
    let GraphResult::ScopeDetails { value } = value.result else {
        panic!("owned status")
    };
    assert!(!value.current);
    assert!(value.unavailable_reason.is_some());
    let revision = read(&*store, |s| {
        devworks_core::services::indexing::section_config(
            s.indexing(),
            &devworks_core::services::indexing::section_key(None),
        )
    })
    .revision;
    let refresh = json!({"protocol":"1.0-draft.1","schemaRevision":2,"requestKey":"owned-overlay","command":{"kind":"RefreshOverlay","repositoryId":repository,"worktreeId":tree.worktree_id,"expectedSourceRevision":revision.to_string()}});
    let reply = app
        .command(
            &user,
            "indexing.refresh_overlay",
            refresh.to_string().as_bytes(),
        )
        .unwrap();
    let CommandReply::Recorded { receipt, .. } = reply else {
        panic!("{reply:?}")
    };
    let DomainDecision::Applied {
        result: OperationResult::IndexingRefreshOverlay { value },
    } = receipt.decision
    else {
        panic!("{:?}", receipt.decision)
    };
    executor.block_on(within("owned overlay publishes", async {
        loop {
            let state = run_state(&store, value.id.as_str());
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let answer = app
        .query(&context, "code.source", frame.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::CodeSource { value },
        ..
    } = answer
    else {
        panic!("{answer:?}")
    };
    let owned_pin=value.view.as_ref().unwrap().pin_id.clone();
    assert_eq!(
        value.view.unwrap().views[0].snapshot.worktree_id.as_ref(),
        Some(&tree.worktree_id)
    );
    let GraphResult::SourceText { value } = value.result else {
        panic!("owned source")
    };
    assert_eq!(value.text.0, "owned dirty capture\n");
    write(&*store, |tx| {
        let mut tree = tx.git().git_worktree(&id("git-owned-record"))?.unwrap();
        let expected = tree.revision;
        tree.revision += 1;
        tree.ownership_revision += 1;
        tree.owner=devworks_core::protocol::ActorId(id("user"));
        tx.git().update_git_worktree(&tree, expected)
    })
    .unwrap();
    let stale = app
        .query(
            &context,
            "code.graph_scope_status",
            status.to_string().as_bytes(),
        )
        .unwrap();
    assert!(
        matches!(&stale,DomainQueryReply::Rejected{fault,..}if fault.code==devworks_core::protocol::DomainErrorCode::PermissionDenied),
        "{stale:?}"
    );
    let owner=context.principal().clone();
    write(&*store,move|tx|{let mut tree=tx.git().git_worktree(&id("git-owned-record"))?.unwrap();let expected=tree.revision;tree.owner=owner;tree.revision+=1;tree.ownership_revision+=1;tx.git().update_git_worktree(&tree,expected)}).unwrap();
    let rebound=devworks_core::services::git::context::bind_worktree(&*store,context.clone(),&id("git-owned-record")).unwrap();
    let mut old_pin=frame.clone();old_pin.as_object_mut().unwrap().remove("current");old_pin["view"]=json!({"kind":"Pinned","pinId":owned_pin});
    let denied=app.query(&rebound,"code.source",old_pin.to_string().as_bytes()).unwrap();assert!(matches!(&denied,DomainQueryReply::Rejected{fault,..}if fault.code==devworks_core::protocol::DomainErrorCode::ViewUnavailable),"an ABA owner cannot inherit an earlier custody pin: {denied:?}");
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}
