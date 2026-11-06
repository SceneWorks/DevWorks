use super::*;
use devworks_core::{
    ports::storage::{ConfigScope, StorageError, WriteOptions, run_write},
    protocol::{
        data::GraphResult,
        records::{IndexFreshnessState, RepositoryRecord},
    },
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
struct Fx {
    root: Root,
    engine: Engine,
    repo: PathBuf,
    record: RepositoryRecord,
}
impl Fx {
    fn new(name: &str) -> Self {
        let root = Root::new(name);
        let base = devworks_core::ports::indexing::SourceTree::canonical_dir(
            &devworks_adapters::index::NativeSourceTree::new(),
            &root.dir("sources"),
        )
        .unwrap();
        let repo = base.join("svc");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let mut rust = String::from(
            "pub fn target() {}\npub fn caller(){target();}\npub fn outer(){caller();}\n",
        );
        for i in 0..120 {
            rust.push_str(&format!("pub fn item_{i}() {{}}\n"));
        }
        std::fs::write(repo.join("src/lib.rs"), rust).unwrap();
        std::fs::write(
            repo.join("README.md"),
            "# Original guide\nCaptured repository instructions.\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("long.txt"),
            format!("{}\nsecond line\n", "世界".repeat(40000)),
        )
        .unwrap();
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
        assert_eq!(records.len(), 1);
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
    click(&mut h, Role::Button, "Help for current screen");
    step_until(
        &mut h,
        "captured graph context documentation",
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
        tx.indexing().update_generation(&StoredGeneration{id:generation.clone(),repository_id:repository.clone(),layer:LAYER_BASELINE.into(),state:GEN_SEALED.into(),source_digest:"a".repeat(40),watermark:2,manifest:json!({})})?;
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
}
