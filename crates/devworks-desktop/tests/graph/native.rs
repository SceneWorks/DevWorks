use super::*;
use devworks_core::{
    ports::storage::{
        ConfigScope, DiagnosticFilter, ReadOptions, StorageError, WriteOptions, read_snapshot,
        run_write,
    },
    protocol::{
        data::GraphResult,
        records::{IndexFreshnessState, RepositoryRecord},
    },
    services::diagnostics::channels::REPOSITORY_INDEXING,
    settings::catalog::descriptor,
};
use devworks_desktop::{backend::Backend, host::Engine, ops, shell::Shell};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};
fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
/// What the runtime recorded about its discovery scans (shown when a discovery assertion fails).
fn discovery_problems(engine: &Engine) -> Vec<String> {
    read_snapshot(
        &**engine.host().runtime().store(),
        &ReadOptions::default(),
        |s| {
            s.diagnostics().diagnostics(
                &DiagnosticFilter {
                    component: Some(REPOSITORY_INDEXING.into()),
                    operation: Some("indexing.discover".into()),
                    ..DiagnosticFilter::default()
                },
                50,
            )
        },
    )
    .map_or_else(
        |e| vec![format!("diagnostics unreadable: {e}")],
        |rows| {
            rows.into_iter()
                .map(|d| format!("{}: {}", d.severity, d.message))
                .collect()
        },
    )
}
struct Fx {
    root: Root,
    engine: Engine,
    repo: PathBuf,
    record: RepositoryRecord,
}
impl Fx {
    fn new(name: &str) -> Self {
        let mut rust = String::from(
            "pub fn target() {}\npub fn caller(){target();}\npub fn outer(){caller();}\n",
        );
        for i in 0..120 {
            rust.push_str(&format!("pub fn item_{i}() {{}}\n"));
        }
        Self::with(
            name,
            &[
                ("src/lib.rs", rust),
                (
                    "README.md",
                    "# Original guide\nCaptured repository instructions.\n".into(),
                ),
                (
                    "long.txt",
                    format!("{}\nsecond line\n", "世界".repeat(40000)),
                ),
                ("empty.txt", String::new()),
            ],
        )
    }
    /// A registered, discovered repository `svc` of `files` (path and text).
    fn with(name: &str, files: &[(&str, String)]) -> Self {
        let root = Root::new(name);
        let base = devworks_core::ports::indexing::SourceTree::canonical_dir(
            &devworks_adapters::index::NativeSourceTree::new(),
            &root.dir("sources"),
        )
        .unwrap();
        let repo = base.join("svc");
        for (path, text) in files {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "original"]);
        let engine = start(&root);
        let d = descriptor("repositories.baseDirectory").unwrap();
        let assignment=serde_json::from_value(json!({"key":"repositories.baseDirectory","value":{"schema":d.schema_ref(),"value":base.display().to_string()}})).unwrap();
        let user = engine.host().user().clone();
        run_write(
            &**engine.host().runtime().store(),
            WriteOptions::default(),
            move |tx| {
                devworks_core::settings::service::apply_human_assignments(
                    tx,
                    &user,
                    &ConfigScope::Workspace,
                    &[assignment],
                    json!({}),
                )
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            },
        )
        .unwrap();
        let b = engine.backend();
        let (records, more) =
            ops::discover_result(b.query("indexing.discover", &ops::discover(&*b))).unwrap();
        assert!(!more);
        assert_eq!(
            records.len(),
            1,
            "recorded discovery problems: {:?}",
            discovery_problems(&engine)
        );
        Self {
            root,
            engine,
            repo,
            record: records.into_iter().next().unwrap(),
        }
    }
    fn index(&self, rebuild: bool) {
        let b = self.engine.backend();
        if rebuild {
            // Re-observe the actual repository head before requesting a new immutable index.
            ops::discover_result(b.query("indexing.discover", &ops::discover(&*b))).unwrap();
        }
        let op = if rebuild {
            "indexing.reindex"
        } else {
            "indexing.index"
        };
        ops::index_run_result(b.command(
            op,
            &ops::index_repository(
                &*b,
                self.record.id.as_str(),
                rebuild,
                b.index_sources(None).unwrap().revision,
            ),
        ))
        .unwrap();
        let until = Instant::now() + SETTLE_BOUND;
        loop {
            let status = ops::index_status_result(b.query(
                "indexing.status",
                &ops::index_status(&*b, self.record.id.as_str()),
            ))
            .unwrap();
            if status
                .freshness
                .as_ref()
                .is_some_and(|f| matches!(f.state, IndexFreshnessState::Current))
            {
                break;
            }
            assert!(Instant::now() < until, "actual index publishes: {status:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn ui(&self, backend: Arc<dyn Backend>) -> egui_kittest::Harness<'static, Shell> {
        let mut h = harness(
            shell_with(Ok(backend), &self.root, Script::default(), None),
            1300.0,
            950.0,
        );
        settle(&mut h);
        h.state_mut()
            .chat
            .drafts
            .entry(devworks_desktop::chat::NEW_DRAFT.into())
            .or_default()
            .text = "Keep graph exploration draft".into();
        h.state_mut()
            .open_repository_graph(self.record.id.as_str(), "svc");
        settle(&mut h);
        h
    }
}
fn pin(h: &egui_kittest::Harness<'_, Shell>) -> String {
    h.state()
        .graph
        .pin
        .as_ref()
        .expect("real index yields retained pin")
        .pin_id
        .as_str()
        .into()
}
fn graph_tab(h: &mut egui_kittest::Harness<'_, Shell>, label: &str) {
    settle(h);
    h.query_all_by_role_and_label(Role::Button, label)
        .find(|node| node.rect().left() > devworks_desktop::theme::NAV)
        .expect("repository pane tab")
        .click();
    settle(h);
}
fn read(h: &mut egui_kittest::Harness<'_, Shell>, label: &str, operation: &str, expected: &str) {
    click(h, Role::Button, label);
    assert!(
        h.state().graph.error.is_none(),
        "{operation}: {:?}",
        h.state().graph.error
    );
    let read = h.state().graph.read.as_ref().unwrap();
    assert_eq!(read.operation, operation);
    let page = read
        .page
        .as_ref()
        .expect("actual gateway returns typed page");
    if let Some(view) = &page.view {
        assert_eq!(
            view.pin_id.as_str(),
            expected,
            "{operation} preserves the captured set"
        );
    }
    assert_eq!(pin(h), expected);
}
#[test]
fn native_indexed_graph_exposes_all_operations_and_recovers_exact_trust_request() {
    let f = Fx::new("graph-native-operators");
    f.index(false);
    let lost = LoseAcks::over(
        f.engine.backend(),
        "code.set_trust",
        devworks_desktop::backend::UNKNOWN_RESUBMISSIONS + 1,
    );
    let mut h = f.ui(lost.clone());
    let captured = pin(&h);
    assert!(has_text(&h, "Read captured README"));
    assert!(
        !has_text(&h, "Foundational: No"),
        "unknown repository metadata must not become a negative fact"
    );
    wait_for_screen_help(&f.engine, &f.root, "code-exploration", "code-exploration");
    click(&mut h, Role::Button, "Help for current screen");
    step_until_progressing(
        &mut h,
        "captured graph context documentation",
        || f.root.write_progress(),
        |s| matches!(&s.help.read,Some(devworks_core::protocol::records::HelpReadView::Available{topic,..})if topic.id.as_str()=="code-exploration"),
    );
    assert!(has_text(&h, "Explore repository code"));
    click(&mut h, Role::Button, "Back to previous screen");
    assert_eq!(pin(&h), captured);
    read(
        &mut h,
        "Repository architecture",
        "code.architecture",
        &captured,
    );
    assert!(has_text(
        &h,
        "No repository analysis summary has been recorded"
    ));
    read(&mut h, "Browse nodes", "repositories.nodes", &captured);
    assert!(
        h.state()
            .graph
            .read
            .as_ref()
            .unwrap()
            .page
            .as_ref()
            .unwrap()
            .next_cursor
            .is_some()
    );
    read(&mut h, "Next graph page", "repositories.nodes", &captured);
    read(
        &mut h,
        "Previous graph page",
        "repositories.nodes",
        &captured,
    );
    h.state_mut().graph.search_mode = 1;
    h.state_mut().graph.filter = "target".into();
    read(&mut h, "Search graph", "code.search", &captured);
    h.state_mut().graph.search_mode = 2;
    h.state_mut().graph.filter = "svc".into();
    read(&mut h, "Search graph", "code.search", &captured);
    assert!(
        !has_text(&h, "Foundational: No"),
        "Unified search preserves unknown foundational roles"
    );
    assert!(
        matches!(&h.state().graph.read.as_ref().unwrap().page.as_ref().unwrap().result,GraphResult::CodeSearch{value} if value.items.0.iter().any(|hit|matches!(hit,devworks_core::protocol::records::CodeSearchHit::Repository{..})))
    );
    h.state_mut().graph.search_mode = 0;
    h.state_mut().graph.filter = "target".into();
    read(&mut h, "Search graph", "code.search", &captured);
    read(&mut h, "target", "code.node_detail", &captured);
    let selected = h.state().graph.selected.as_ref().unwrap().clone();
    assert!(selected.source.is_some());
    read(&mut h, "Read node source", "code.source", &captured);
    read(&mut h, "Code snippet", "code.snippet", &captured);
    read(&mut h, "Read whole file", "code.source", &captured);
    click(
        &mut h,
        Role::DisclosureTriangle,
        "Explore calls, impact and data flow",
    );
    for (label, op) in [
        ("Calls", "code.call_path"),
        ("Impact", "code.impact"),
        ("Data lineage", "code.data_lineage"),
        ("Consumers", "code.consumers"),
        ("Publishers", "code.publishers"),
        ("Relationships", "code.traverse"),
        ("Node dependencies", "code.dependency"),
    ] {
        read(&mut h, label, op, &captured);
    }
    click(&mut h, Role::DisclosureTriangle, "Explore a source file");
    h.state_mut().graph.file = "src/lib.rs".into();
    read(&mut h, "Read line window", "code.source", &captured);
    read(&mut h, "Nodes in file", "code.nodes_by_file", &captured);
    read(&mut h, "File impact", "code.file_impact", &captured);
    read(
        &mut h,
        "Dependencies",
        "code.repository_dependencies",
        &captured,
    );
    read(&mut h, "Graph schema", "code.graph_schema", &captured);
    read(
        &mut h,
        "Index coverage",
        "code.graph_scope_status",
        &captured,
    );
    // Name lookup is also shared service behavior, including captured-scope rather than only
    // selected-repository resolution. This is an actual button request, not a forged page.
    h.state_mut().graph.target_name = "target".into();
    h.state_mut().graph.target_all = true;
    read(&mut h, "Calls", "code.call_path", &captured);
    h.state_mut().graph.target_name.clear();
    read(&mut h, "Search graph", "code.search", &captured);
    read(&mut h, "target", "code.node_detail", &captured);
    click(&mut h, Role::Button, "Mark untrusted");
    assert!(h.state().graph.trust_unknown);
    assert_eq!(
        lost.frames.lock().unwrap().len(),
        devworks_desktop::backend::UNKNOWN_RESUBMISSIONS + 1
    );
    let original = lost.frames.lock().unwrap()[0].clone();
    click(&mut h, Role::Button, "Confirm outcomes");
    assert!(!h.state().graph.trust_unknown);
    assert!(h.state().graph.selected.as_ref().unwrap().do_not_trust);
    assert_eq!(pin(&h), captured);
    assert!(
        lost.frames
            .lock()
            .unwrap()
            .iter()
            .all(|frame| frame == &original)
    );
    let frame: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(frame["command"]["pinId"], captured);
    assert_eq!(frame["command"]["scope"], json!({"kind":"Workspace"}));
    assert_eq!(frame["command"]["unitKey"], json!(selected.unit_key));
    assert_eq!(
        frame["command"]["expectedContentIdentity"],
        selected.content_identity.0
    );
    click(&mut h, Role::Button, "Back to repositories");
    click_nav(&mut h, "Chat");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep graph exploration draft")
    );
}
#[test]
fn native_readme_long_lines_and_refresh_keep_one_capture_until_explicit_refresh() {
    let f = Fx::new("graph-native-capture");
    let mut h = f.ui(f.engine.backend());
    h.ctx
        .set_missing_glyph_policy(eframe::egui::MissingGlyphPolicy::Tofu);
    assert!(h.state().graph.pin.is_none());
    assert!(has_text(&h, "Indexed totals are unknown"));
    click(&mut h, Role::Button, "Graph schema");
    assert!(h.state().graph.error.is_none());
    assert!(h.state().graph.pin.is_none());
    f.index(false);
    click(&mut h, Role::Button, "Refresh selected graph view");
    let captured = pin(&h);
    // The last schema query remains schema; opening Overview explicitly recaptures no new pin.
    graph_tab(&mut h, "Overview");
    read(&mut h, "Read captured README", "code.source", &captured);
    assert!(has_text(&h, "Original guide"));
    click(&mut h, Role::Button, "Back to previous graph view");
    std::fs::write(f.repo.join("README.md"), "# New guide\nNew capture only.\n").unwrap();
    git(&f.repo, &["add", "-A"]);
    git(&f.repo, &["commit", "-q", "-m", "new guide"]);
    f.index(true);
    read(&mut h, "Read captured README", "code.source", &captured);
    assert!(has_text(&h, "Original guide"));
    assert!(!has_text(&h, "New guide"));
    click(&mut h, Role::DisclosureTriangle, "Explore a source file");
    h.state_mut().graph.file = "long.txt".into();
    read(&mut h, "Read whole source file", "code.source", &captured);
    // Test UI uses the app's missing-glyph fallback for real Unicode, rather than kittest's
    // default panic policy; exact passage bytes remain asserted from the gateway projection.
    let mut bytes = Vec::new();
    loop {
        let page = h
            .state()
            .graph
            .read
            .as_ref()
            .unwrap()
            .page
            .as_ref()
            .unwrap();
        let GraphResult::SourceText { value } = &page.result else {
            panic!("actual source result")
        };
        bytes.extend_from_slice(value.text.0.as_bytes());
        if page.next_cursor.is_none() {
            break;
        }
        read(&mut h, "Next graph page", "code.source", &captured);
    }
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        format!("{}\nsecond line\n", "世界".repeat(40000))
    );
    h.state_mut().graph.file = "empty.txt".into();
    read(&mut h, "Read whole source file", "code.source", &captured);
    assert!(has_text(&h, "This captured file is empty"));
    graph_tab(&mut h, "Overview");
    click(&mut h, Role::Button, "Refresh selected graph view");
    let refreshed = pin(&h);
    assert_ne!(captured, refreshed);
    read(&mut h, "Read captured README", "code.source", &refreshed);
    assert!(has_text(&h, "New guide"));
    let view = h.state().graph.pin.as_ref().unwrap().views[0]
        .view_id
        .as_str()
        .to_owned();
    h.state_mut().graph.view_mode = 2;
    h.state_mut().graph.exact_views = std::collections::BTreeSet::from([view.clone()]);
    click(&mut h, Role::Button, "Refresh selected graph view");
    assert!(h.state().graph.error.is_none());
    assert_eq!(
        h.state().graph.pin.as_ref().unwrap().views[0]
            .view_id
            .as_str(),
        view
    );
    h.state_mut()
        .graph
        .exact_views
        .insert("missing-indexed-view".into());
    click(&mut h, Role::Button, "Refresh selected graph view");
    assert!(
        h.state().graph.pin.is_none(),
        "strict Exact selection cannot adopt a published fallback"
    );
    assert!(h.state().graph.error.is_some());
    h.state_mut().graph.view_mode = 1;
    click(&mut h, Role::Button, "Refresh selected graph view");
    assert!(h.state().graph.error.is_none());
    assert_eq!(
        h.state().graph.pin.as_ref().unwrap().views[0]
            .view_id
            .as_str(),
        view
    );
}

#[test]
fn native_project_selection_fences_graph_and_current_scope_reports_missing_indexes() {
    let f = Fx::new("graph-native-project");
    f.index(false);
    let missing = f.repo.parent().unwrap().join("missing");
    std::fs::create_dir_all(&missing).unwrap();
    git(&missing, &["init", "-q"]);
    git(
        &missing,
        &["commit", "-q", "--allow-empty", "-m", "unindexed"],
    );
    let b = f.engine.backend();
    let records = ops::discover_result(b.query("indexing.discover", &ops::discover(&*b)))
        .unwrap()
        .0;
    let missing_record = records.iter().find(|r| r.id != f.record.id).unwrap();
    let a = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(&*b, "Indexed project", "", &[f.record.id.as_str().into()]),
    ))
    .unwrap();
    let z = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(
            &*b,
            "Unindexed project",
            "",
            &[missing_record.id.as_str().into()],
        ),
    ))
    .unwrap();
    let mut h = f.ui(b);
    let page = h
        .state()
        .graph
        .read
        .as_ref()
        .unwrap()
        .page
        .as_ref()
        .unwrap();
    assert!(
        page.scope_coverage
            .as_ref()
            .unwrap()
            .unavailable_repositories
            .0
            .iter()
            .any(|gap| gap.repository_id == missing_record.id)
    );
    assert!(has_text(&h, "NotIndexed"));
    let workspace_pin = pin(&h);
    read(
        &mut h,
        "Captured scope overview",
        "code.overview",
        &workspace_pin,
    );
    assert!(
        h.state()
            .graph
            .read
            .as_ref()
            .unwrap()
            .page
            .as_ref()
            .unwrap()
            .scope_coverage
            .as_ref()
            .unwrap()
            .unavailable_repositories
            .0
            .iter()
            .any(|gap| gap.repository_id == missing_record.id)
    );
    h.state_mut().select_project(Some(a.id.0.as_str().into()));
    assert!(!h.state().graph.opened);
    h.state_mut()
        .open_repository_graph(f.record.id.as_str(), "svc");
    settle(&mut h);
    let captured = pin(&h);
    assert_eq!(h.state().graph.project.as_deref(), Some(a.id.0.as_str()));
    read(&mut h, "Browse nodes", "repositories.nodes", &captured);
    assert!(
        h.state()
            .graph
            .read
            .as_ref()
            .unwrap()
            .page
            .as_ref()
            .unwrap()
            .scope_coverage
            .as_ref()
            .unwrap()
            .unavailable_repositories
            .0
            .is_empty()
    );
    h.state_mut().select_project(Some(z.id.0.as_str().into()));
    assert!(!h.state().graph.opened);
    h.state_mut()
        .open_repository_graph(missing_record.id.as_str(), "missing");
    settle(&mut h);
    assert!(h.state().graph.pin.is_none());
    assert!(has_text(&h, "Indexed totals are unknown"));
    assert_eq!(h.state().graph.project.as_deref(), Some(z.id.0.as_str()));
    h.state_mut().select_project(None);
    assert!(!h.state().graph.opened);
    click_nav(&mut h, "Chat");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep graph exploration draft")
    );
}

#[test]
fn native_recorded_module_pages_are_retained_without_inventing_source_or_program_totals() {
    use devworks_core::ports::storage::*;
    let root = Root::new("graph-native-recorded");
    let engine = start(&root);
    // A sealed SQLite index payload represents a previously recorded analysis. This tests
    // the actual read projection and native reader, without pretending to run its producer.
    run_write(&**engine.host().runtime().store(),WriteOptions::default(),|tx|{
        let id=|s:&str|devworks_core::protocol::Id::try_from(s.to_owned()).unwrap();
        let repository=id("recorded-repository");let generation=id("recorded-generation");let view=id("recorded-view");
        tx.projects().insert_repository(&StoredRepository{id:repository.clone(),location:"/captured/recorded".into(),name:"Recorded".into(),remote_url:None,primary:true,lifecycle:"Active".into(),languages:vec!["csharp".into()],revision:1,registered_at_ms:1})?;
        tx.indexing().insert_generation(&StoredGeneration{id:generation.clone(),repository_id:repository.clone(),layer:LAYER_BASELINE.into(),state:GEN_STAGING.into(),source_digest:"a".repeat(40),watermark:0,manifest:json!({})})?;
        let key="Recorded.csproj";
        let endpoints:Vec<_>=(0..650).map(|i|json!({"route":format!("/recorded/{i}"),"httpMethod":"GET","description":"Recorded endpoint"})).collect();
        let recorded=json!({"summary":"Recorded source summary","confidence":"high","endpoints":endpoints,"services":[],"externalDependencies":[],"databaseTables":[],"updatedAtMs":"17","modelUsed":"recorded-producer","programTotals":{"endpointCount":"999","serviceCount":"2","tableCount":"3"}});
        tx.indexing().put_unit(&StoredUnit{generation_id:generation.clone(),unit_key:key.into(),content_digest:"opaque-recorded-content".into(),action:UNIT_REPLACE.into(),payload:json!({})},&[
            StoredNode{unit_key:key.into(),node_id:"module-recorded".into(),kind:"DotnetProject".into(),name:"Recorded".into(),payload:json!({"properties":{"dotnet_project":"Recorded","analysis":recorded}})}
        ],&[])?;
        tx.indexing().put_unit(&StoredUnit{generation_id:generation.clone(),unit_key:"Other.csproj".into(),content_digest:"opaque-other-content".into(),action:UNIT_REPLACE.into(),payload:json!({})},&[
            StoredNode{unit_key:"Other.csproj".into(),node_id:"module-other".into(),kind:"DotnetProject".into(),name:"Other".into(),payload:json!({"properties":{"dotnet_project":"Other"}})}
        ],&[])?;
        tx.indexing().put_unit(&StoredUnit{generation_id:generation.clone(),unit_key:"/".into(),content_digest:"opaque-root-content".into(),action:UNIT_REPLACE.into(),payload:json!({})},&[
            StoredNode{unit_key:"/".into(),node_id:"recorded-root".into(),kind:"Repository".into(),name:"Recorded".into(),payload:json!({"properties":{"foundational":true}})}
        ],&[])?;
        tx.indexing().update_generation(&StoredGeneration{id:generation.clone(),repository_id:repository.clone(),layer:LAYER_BASELINE.into(),state:GEN_SEALED.into(),source_digest:"a".repeat(40),watermark:3,manifest:json!({})})?;
        tx.indexing().insert_view(&StoredView{view_id:view.clone(),repository_id:repository.clone(),view_digest:devworks_core::fingerprint::sha256_hex(b"recorded-view"),baseline_id:generation,branch_id:None,worktree_id:None,coverage:json!({"state":"Complete","commit":"a".repeat(40),"sourceRevision":1})})?;
        tx.indexing().publish(&repository,&view,0)?;
        tx.projects().publish_index_view(&StoredIndexView{view_id:view,repository_id:repository,source_revision:"a".repeat(40),coverage:"Complete".into(),published_at_ms:1})?;
        Ok(())
    }).unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1200.0, 850.0);
    settle(&mut h);
    h.state_mut()
        .open_repository_graph("recorded-repository", "Recorded");
    settle(&mut h);
    let captured = pin(&h);
    assert!(has_text(&h, "Foundational: Yes"));
    read(
        &mut h,
        "Repository architecture",
        "code.architecture",
        &captured,
    );
    assert!(has_text(&h, "Recorded source summary"));
    assert!(!has_text(&h, "Program totals have not been recorded"));
    assert!(has_text(
        &h,
        "No semantic analysis has been recorded for this module"
    ));
    assert!(has_text(&h, "999 endpoints"));
    for _ in 0..30 {
        if h.state()
            .graph
            .read
            .as_ref()
            .unwrap()
            .page
            .as_ref()
            .unwrap()
            .next_cursor
            .is_none()
        {
            break;
        }
        read(&mut h, "Next graph page", "code.architecture", &captured);
    }
    let count = |h: &egui_kittest::Harness<'_, Shell>| {
        h.state()
            .graph
            .read
            .as_ref()
            .unwrap()
            .module_pages
            .iter()
            .flat_map(|(_, p)| devworks_desktop::graph_screen::module_items(p))
            .filter_map(|item| item.recorded.as_ref())
            .map(|a| a.endpoints.0.len())
            .sum::<usize>()
    };
    assert_eq!(
        count(&h),
        650,
        "all recorded endpoint rows survive native paging"
    );
    assert!(has_text(&h, "/recorded/649"));
    read(
        &mut h,
        "Previous graph page",
        "code.architecture",
        &captured,
    );
    assert_eq!(count(&h), 650, "revisiting a cursor cannot duplicate rows");
    read(
        &mut h,
        "Inspect Recorded module",
        "code.node_detail",
        &captured,
    );
    assert!(has_text(&h, "No source location is retained for this node"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Read node source")
            .accesskit_node()
            .is_disabled()
    );
    assert!(h.state().graph.selected.as_ref().unwrap().source.is_none());
    assert_eq!(
        h.state()
            .graph
            .selected
            .as_ref()
            .unwrap()
            .content_identity
            .0,
        "opaque-recorded-content"
    );
    click(&mut h, Role::Button, "Back to previous graph view");
    read(
        &mut h,
        "Browse Recorded module",
        "repositories.nodes",
        &captured,
    );
    let kind = match &h
        .state()
        .graph
        .read
        .as_ref()
        .unwrap()
        .page
        .as_ref()
        .unwrap()
        .result
    {
        GraphResult::CodeNodes { value } => {
            assert!(!value.nodes.0.is_empty());
            assert!(value.nodes.0.iter().all(|node| {
                node.internal_module
                    .as_ref()
                    .is_some_and(|m| m.0 == "Recorded")
            }));
            value.nodes.0[0].node_kind.0.clone()
        }
        other => panic!("actual filtered node page: {other:?}"),
    };
    h.state_mut().graph.node_kind = kind.clone();
    read(&mut h, "Browse nodes", "repositories.nodes", &captured);
    assert!(
        matches!(&h.state().graph.read.as_ref().unwrap().page.as_ref().unwrap().result,GraphResult::CodeNodes{value}if value.nodes.0.iter().all(|node|node.node_kind.0==kind))
    );
}

/// A schema read of `op` at revision 5, answered.
fn schema_read(engine: &Engine, op: &str, body: serde_json::Value) -> serde_json::Value {
    use devworks_core::protocol::results::DomainQueryReply;
    let b = engine.backend();
    match b
        .query(op, &devworks_desktop::databases_ops::query(&*b, op, body))
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("{op}: {other:?}"),
    }
}
/// A schema command of `op` at revision 5, applied.
fn schema_command(engine: &Engine, op: &str, body: serde_json::Value) -> serde_json::Value {
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let b = engine.backend();
    let CommandReply::Recorded { receipt, .. } = b
        .command(op, &devworks_desktop::databases_ops::command(&*b, op, body))
        .unwrap()
    else {
        panic!("{op}: no receipt")
    };
    match receipt.decision {
        DomainDecision::Applied { result } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        // A capture runs in the background; the caller waits for its view.
        DomainDecision::Accepted { .. } => serde_json::Value::Null,
        other => panic!("{op}: {other:?}"),
    }
}

/// Indexing code that queries a table a schema source knows records the link with its evidence,
/// and both the schema reads and the graph's node detail return it.
#[test]
fn indexed_code_that_queries_a_known_table_links_to_it_in_schema_and_graph_reads() {
    let _native_case = native_runtime_case();
    let f = Fx::with(
        "graph-schema-links",
        &[(
            "src/orders.py",
            "class Order(Base):\n    __tablename__ = \"orders\"\n\ndef load(cur, order_id):\n    return cur.execute(\"SELECT o.id, o.total FROM orders o WHERE o.id = ?\", (order_id,))\n\ndef label(text):\n    return \"Select an order from the list\"\n".into(),
        )],
    );
    f.index(false);
    // The known table: a SQLite source with `orders`, indexed and published.
    let path = f.root.file("shop.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE orders(id INTEGER PRIMARY KEY, total INTEGER); CREATE TABLE refunds(id INTEGER);")
        .unwrap();
    let source = schema_command(
        &f.engine,
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":{"name":"Shop","engine":"Sqlite","endpoint":path,
            "catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}}),
    );
    let connection = source["id"].as_str().unwrap().to_owned();
    schema_command(
        &f.engine,
        "schemas.refresh",
        json!({"connectionId":connection,"expectedRevision":source["revision"],"scope":{"kind":"Workspace"},"testOnly":false}),
    );
    let until = Instant::now() + SETTLE_BOUND;
    while schema_read(
        &f.engine,
        "schemas.connections",
        json!({"scope":{"kind":"Workspace"},"page":{"limit":10}}),
    )["items"][0]["currentViewId"]
        .is_null()
    {
        assert!(Instant::now() < until, "the schema view publishes");
        std::thread::sleep(Duration::from_millis(20));
    }
    let views = json!({"kind":"SchemaPublished","connectionIds":[connection]});
    let objects = schema_read(
        &f.engine,
        "schemas.objects",
        json!({"scope":{"kind":"Workspace"},"views":views,"connectionId":connection,"kinds":[],"text":"orders","page":{"limit":100}}),
    );
    let named = |objects: &serde_json::Value, kind: &str, name: &str| {
        objects["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["kind"] == kind && o["metadata"]["name"] == name)
            .unwrap_or_else(|| panic!("{kind} {name} in {objects}"))["id"]
            .clone()
    };
    let table = named(&objects, "Table", "orders");
    let columns = schema_read(
        &f.engine,
        "schemas.objects",
        json!({"scope":{"kind":"Workspace"},"views":views,"connectionId":connection,"kinds":["Column"],"parentId":table,"page":{"limit":100}}),
    );
    let total = named(&columns, "Column", "total");

    // The schema read returns each use of the table with its evidence.
    let references = schema_read(
        &f.engine,
        "schemas.code_references",
        json!({"scope":{"kind":"Workspace"},"views":views,"connectionId":connection,"objectId":table,"page":{"limit":100}}),
    );
    let items = references["items"].as_array().unwrap();
    let by = |extractor: &str| {
        items
            .iter()
            .find(|r| r["extractor"] == extractor)
            .unwrap_or_else(|| panic!("a {extractor} use of orders: {references}"))
    };
    let literal = by("sql-literal");
    assert_eq!(literal["nodeName"], "load", "{literal}");
    assert_eq!(literal["unitKey"], "src/orders.py");
    assert_eq!(literal["line"], "5");
    assert_eq!(literal["statement"], "select");
    assert_eq!(literal["edgeKind"], "USES_TABLE");
    assert_eq!(literal["repositoryId"], f.record.id.as_str());
    assert!(
        literal["snippet"]
            .as_str()
            .unwrap()
            .contains("FROM orders o")
    );
    let mapping = by("orm:sqlalchemy");
    assert_eq!(mapping["nodeName"], "Order", "{mapping}");
    assert_eq!(items.len(), 2, "prose is not a query: {references}");
    // A column's read returns only the uses that name it.
    let columns = schema_read(
        &f.engine,
        "schemas.code_references",
        json!({"scope":{"kind":"Workspace"},"views":views,"connectionId":connection,"objectId":total,"page":{"limit":100}}),
    );
    let columns = columns["items"].as_array().unwrap();
    assert_eq!(columns.len(), 1, "{columns:?}");
    assert_eq!(columns[0]["column"], "total");
    assert_eq!(columns[0]["edgeKind"], "USES_COLUMN");
    // A table no code names has no uses.
    let refunds = schema_read(
        &f.engine,
        "schemas.objects",
        json!({"scope":{"kind":"Workspace"},"views":views,"connectionId":connection,"kinds":["Table"],"text":"refunds","page":{"limit":10}}),
    )["items"][0]["id"]
        .clone();
    assert!(
        schema_read(
            &f.engine,
            "schemas.code_references",
            json!({"scope":{"kind":"Workspace"},"views":views,"connectionId":connection,"objectId":refunds,"page":{"limit":100}}),
        )["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // The graph's node detail of `load` returns the same link to the file's table node.
    let b = f.engine.backend();
    let detail = {
        use devworks_core::protocol::results::DomainQueryReply;
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"schema-links","scope":{"kind":"Workspace"},"current":true,
            "query":{"kind":"Node","nodeId":literal["nodeId"]},"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
        match b
            .query("code.node_detail", frame.to_string().as_bytes())
            .unwrap()
        {
            DomainQueryReply::Result { result, .. } => {
                serde_json::to_value(result).unwrap()["value"]["result"]["value"].clone()
            }
            other => panic!("node detail: {other:?}"),
        }
    };
    let edge = detail["outgoing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["edgeKind"] == "USES_TABLE")
        .unwrap_or_else(|| panic!("the USES_TABLE edge: {detail}"));
    let property = |name: &str| {
        edge["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .map(|p| p["jsonText"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| panic!("{name} in {edge}"))
    };
    assert_eq!(property("extractor"), "\"sql-literal\"");
    assert_eq!(property("line"), "5");
    assert_eq!(property("table"), "\"orders\"");
    let neighbor = detail["neighbors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["nodeId"] == edge["targetId"])
        .unwrap_or_else(|| panic!("the table node: {detail}"));
    assert_eq!(neighbor["nodeKind"], "DatabaseTable");
    assert_eq!(neighbor["qualifiedName"], "db-table:orders");

    // The graph screen shows the use and leads to the table in Databases.
    let mut h = f.ui(f.engine.backend());
    let captured = pin(&h);
    h.state_mut().graph.search_mode = 0;
    h.state_mut().graph.filter = "load".into();
    read(&mut h, "Search graph", "code.search", &captured);
    read(&mut h, "load", "code.node_detail", &captured);
    assert!(has_text(&h, "Database tables"));
    assert!(
        has_text(&h, "orders.id · line 5 · sql-literal")
            || has_text(&h, "orders · line 5 · sql-literal")
    );
    click(&mut h, Role::Button, "Find orders in Databases");
    assert_eq!(h.state().route(), Route::Databases);
    assert_eq!(h.state().databases.filter, "orders");
    // Databases lists the code that uses the table and opens a use in the graph.
    step_until(&mut h, "the source is listed", |s| {
        !s.databases.loading && !s.databases.connections.is_empty()
    });
    h.query_by_label("Shop").expect("the source").click();
    settle(&mut h);
    click(&mut h, Role::Button, "Search schema objects");
    step_until(&mut h, "orders is found", |s| {
        !s.databases.read_loading
            && s.databases
                .objects
                .iter()
                .any(|o| o.metadata.as_ref().is_some_and(|m| m.name.0 == "orders"))
    });
    let orders = h
        .state()
        .databases
        .objects
        .iter()
        .find(|o| o.metadata.as_ref().is_some_and(|m| m.name.0 == "orders"))
        .unwrap()
        .clone();
    click(
        &mut h,
        Role::Button,
        &format!("{:?} {}", orders.kind, orders.qualified_name.0),
    );
    click(&mut h, Role::Button, "Find code using this object");
    step_until(&mut h, "the code that uses orders", |s| {
        !s.databases.read_loading && s.databases.code_references.len() == 2
    });
    assert!(has_text(&h, "Used by code"));
    assert!(
        has_text(
            &h,
            "src/orders.py:5 · Function load · USES_TABLE · sql-literal"
        ),
        "{:?}",
        h.state().databases.code_references
    );
    assert!(has_text(&h, "FROM orders o WHERE"));
    click(&mut h, Role::Button, "Open in code graph: load");
    step_until(&mut h, "the use opens in the graph", |s| {
        s.graph
            .read
            .as_ref()
            .is_some_and(|r| r.operation == "code.node_detail" && r.page.is_some())
    });
    assert_eq!(h.state().route(), Route::Repositories);
    assert!(has_text(&h, "Database tables"));
}
