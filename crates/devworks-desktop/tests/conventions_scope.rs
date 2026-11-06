//! Project conventions through the actual desktop Human/runtime ingress, without lineage.
mod common;
use common::{Root, native_runtime_case, start};
use devworks_core::{
    context::PrincipalKind,
    protocol::{ProjectId, provider::ArtifactRef, records::*, results::OperationResult},
};
use devworks_desktop::{backend::Backend, conventions_ops as cop, host::Engine, ops};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

struct Fixture {
    backend: Arc<dyn Backend>,
    engine: Engine,
    project: ProjectId,
    root: Root,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let root = Root::new(name);
        let engine = start(&root);
        assert_eq!(engine.host().user().principal_kind(), PrincipalKind::Human);
        assert!(
            engine.host().user().project_id().is_none(),
            "desktop requests must not acquire a synthetic project lineage"
        );
        let backend = engine.backend();
        let frame = cop::command(
            &*backend,
            "projects.create",
            json!({"name":"Explicit convention project","description":"Native scoped source regression","repositoryIds":[]}),
        );
        let OperationResult::ProjectsCreate { value } =
            ops::command_result(backend.command("projects.create", &frame)).unwrap()
        else {
            panic!("actual project creation")
        };
        Self {
            root,
            engine,
            backend,
            project: value.id,
        }
    }
    fn scope(&self) -> Value {
        json!({"kind":"Project","projectId":self.project})
    }
    fn upload(&self, name: &str, bytes: &[u8], media: &str) -> ArtifactRef {
        let path = self.root.file(name, bytes);
        let ticket = self
            .backend
            .mint_project_upload(&self.project, &path)
            .unwrap();
        let frame = ops::save_project_artifact(
            &*self.backend,
            self.project.0.as_str(),
            ticket.as_str(),
            name,
            media,
            &devworks_core::fingerprint::sha256_hex(bytes),
            bytes.len() as u64,
        );
        ops::saved_artifact(self.backend.command("artifacts.save", &frame))
            .unwrap()
            .artifact
    }
    fn import(&self, resource: bool) -> ConventionRecord {
        self.import_source(resource, "# Scope λ\nscopeguard authored body\n", &[])
    }
    fn import_source(
        &self,
        resource: bool,
        body: &str,
        applicability: &[String],
    ) -> ConventionRecord {
        let raw = "Raw original\0source";
        let bytes = b"\0\xff\x01resource";
        let manifest = json!({"format":"devworks-convention","version":1,"slug":"project-guide","title":"Project source guide","parentPath":[],"lifecycle":"Active","applicability":applicability,"markdown":{"sha256":devworks_core::fingerprint::sha256_hex(body.as_bytes()),"byteLength":body.len().to_string(),"text":body},"rawContent":{"sha256":devworks_core::fingerprint::sha256_hex(raw.as_bytes()),"byteLength":raw.len().to_string(),"text":raw},"resources":if resource {vec![json!({"key":"source-resource","name":"Original resource","mediaType":"application/octet-stream","lifecycle":"Active","sha256":devworks_core::fingerprint::sha256_hex(bytes),"byteLength":bytes.len().to_string(),"base64":"AP8BcmVzb3VyY2U="})]} else {vec![]}});
        let source = self.upload(
            "project-manifest.json",
            &serde_json::to_vec(&manifest).unwrap(),
            "application/json",
        );
        let frame = cop::command(
            &*self.backend,
            "conventions.import",
            json!({"scope":self.scope(),"source":source,"schema":devworks_core::services::conventions::model::manifest_schema()}),
        );
        let cop::Change::Accepted(operation) =
            cop::changed(self.backend.command("conventions.import", &frame)).unwrap()
        else {
            panic!("actual accepted import")
        };
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let status = cop::operation(self.backend.query(
                "conventions.read",
                &cop::query(
                    &*self.backend,
                    "conventions.read",
                    json!({"scope":self.scope(),"operationId":operation}),
                ),
            ))
            .unwrap();
            assert!(
                !matches!(
                    status.phase,
                    ConventionOperationPhase::Failed | ConventionOperationPhase::Cancelled
                ),
                "{status:?}"
            );
            if matches!(status.phase, ConventionOperationPhase::Completed) {
                break;
            }
            assert!(
                Instant::now() < until,
                "actual import did not complete: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let page = cop::page(self.backend.query(
            "conventions.list",
            &cop::query(
                &*self.backend,
                "conventions.list",
                json!({"scope":self.scope(),"page":{"limit":10},"applicability":[]}),
            ),
        ))
        .unwrap();
        assert_eq!(page.items.0.len(), 1);
        page.items.0.into_iter().next().unwrap()
    }
    fn edit(
        &self,
        owner: &ConventionRecord,
        content: &ArtifactRef,
        extra: Value,
    ) -> ConventionRecord {
        let mut body = json!({"scope":self.scope(),"conventionId":owner.id,"expectedRevision":owner.revision,"content":content});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let frame = cop::command(&*self.backend, "conventions.edit", body);
        let cop::Change::Record(record) =
            cop::changed(self.backend.command("conventions.edit", &frame)).unwrap()
        else {
            panic!("actual edited record")
        };
        record
    }
    fn document(&self, owner: &ConventionRecord, format: &str) -> ConventionReadView {
        cop::document(self.backend.query(
            "conventions.read",
            &cop::query(
                &*self.backend,
                "conventions.read",
                json!({"scope":self.scope(),"conventionId":owner.id,"format":format}),
            ),
        ))
        .unwrap()
    }
    fn resources(&self, owner: &ConventionRecord) -> Vec<ConventionResourceRecord> {
        let view=cop::resources(self.backend.query("conventions.resources",&cop::query(&*self.backend,"conventions.resources",json!({"scope":self.scope(),"page":{"limit":10},"applicability":[],"selection":{"kind":"Attachments","conventionId":owner.id,"conventionRevision":owner.revision}})))).unwrap();
        let ConventionResourcesView::Attachments { page } = view else {
            panic!("real attachment metadata")
        };
        page.items.0
    }
}
#[test]
fn project_metadata_edit_reuses_authorized_current_markdown_and_raw_without_lineage() {
    let _native_case = native_runtime_case();
    let f = Fixture::new("convention-scope-current");
    let owner = f.import(false);
    let edited=f.edit(&owner,owner.content.as_ref().unwrap(),json!({"title":"Metadata only title","rawContent":{"kind":"Set","content":owner.raw_content}}));
    assert_eq!(edited.revision.get(), 2);
    assert_eq!(edited.title.0, "Metadata only title");
    assert_eq!(
        f.document(&edited, "Markdown").passage.unwrap().text.0,
        "# Scope λ\nscopeguard authored body\n"
    );
    assert_eq!(
        f.document(&edited, "Raw").passage.unwrap().text.0,
        "Raw original\0source"
    );
    let denied = cop::changed(f.backend.command("conventions.edit", &cop::command(&*f.backend,"conventions.edit",json!({"scope":{"kind":"Workspace"},"conventionId":edited.id,"expectedRevision":edited.revision,"content":edited.content,"title":"Wrong scope must not apply"}))));
    match denied {
        Err(ops::Failure::Refused(fault)) => assert_eq!(
            fault.code,
            devworks_core::protocol::DomainErrorCode::NotFound
        ),
        _ => panic!("an explicit workspace request cannot mutate the project owner"),
    }
    assert_eq!(f.document(&edited, "Markdown").convention.revision.get(), 2);
    assert!(f.engine.host().user().project_id().is_none());
}
#[test]
fn project_resource_capture_reuses_exact_authorized_resource_with_new_body_upload() {
    let _native_case = native_runtime_case();
    let f = Fixture::new("convention-scope-resource");
    let owner = f.import(true);
    let resource = f.resources(&owner).remove(0);
    let body = f.upload(
        "new-markdown.md",
        b"# New body\nscopeguard text",
        "text/markdown",
    );
    let edited=f.edit(&owner,&body,json!({"resources":[{"kind":"Update","resourceId":resource.id,"expectedRevision":resource.revision,"content":resource.content,"name":"Retained exact resource"}]}));
    let actual = f.resources(&edited).remove(0);
    assert_eq!(actual.name.0, "Retained exact resource");
    assert_eq!(actual.content.sha256, resource.content.sha256);
    let result=ops::query_result(f.backend.query("conventions.read",&cop::query(&*f.backend,"conventions.read",json!({"scope":f.scope(),"conventionId":edited.id,"resourceId":actual.id,"format":"Resource"})))).unwrap().0;
    assert_eq!(
        cop::binary(result).unwrap().passage.base64.0,
        "AP8BcmVzb3VyY2U="
    );
}
#[test]
fn project_historical_source_restore_uses_history_scope_instead_of_upload_visibility() {
    let _native_case = native_runtime_case();
    let f = Fixture::new("convention-scope-history");
    let original = f.import(false);
    let next = f.upload(
        "revision-two.md",
        b"# Replacement\nscopeguard second body",
        "text/markdown",
    );
    let replaced = f.edit(&original, &next, json!({}));
    let restored = f.edit(
        &replaced,
        original.content.as_ref().unwrap(),
        json!({"title":"Restored exact original"}),
    );
    assert_eq!(restored.revision.get(), 3);
    assert_eq!(
        f.document(&restored, "Markdown").passage.unwrap().text.0,
        "# Scope λ\nscopeguard authored body\n"
    );
    let versions = cop::page(f.backend.query(
        "conventions.version",
        &cop::query(
            &*f.backend,
            "conventions.version",
            json!({"scope":f.scope(),"conventionId":restored.id,"page":{"limit":10}}),
        ),
    ))
    .unwrap();
    assert_eq!(versions.items.0.len(), 3);
    assert_eq!(
        versions
            .items
            .0
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .sha256,
        original.content.as_ref().unwrap().sha256
    );
}
#[test]
fn project_search_and_section_path_keep_the_authorized_hit_scope_without_lineage() {
    let _native_case = native_runtime_case();
    let f = Fixture::new("convention-scope-breadcrumb");
    let owner = f.import(false);
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let row = f.document(&owner, "Markdown").convention;
        if matches!(
            row.index.status,
            ConventionIndexPhase::Ready | ConventionIndexPhase::LexicalOnly
        ) {
            break;
        }
        assert!(
            !matches!(row.index.status, ConventionIndexPhase::Failed),
            "{row:?}"
        );
        assert!(
            Instant::now() < until,
            "actual body index did not publish: {:?}",
            row.index
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let page = cop::search(f.backend.query(
        "conventions.search",
        &cop::query(
            &*f.backend,
            "conventions.search",
            json!({"scope":f.scope(),"page":{"limit":10},"applicability":[],"text":"scopeguard"}),
        ),
    ))
    .unwrap();
    assert!(!page.items.0.is_empty(), "actual lexical corpus hit");
    let hit = &page.items.0[0];
    assert_eq!(hit.convention.id, owner.id);
    let view=cop::document(f.backend.query("conventions.read",&cop::query(&*f.backend,"conventions.read",json!({"scope":f.scope(),"conventionId":owner.id,"format":"SectionPath","sectionPath":hit.indexed_path})))).unwrap();
    assert_eq!(
        view.passage.unwrap().text.0,
        "Project source guide > # Scope λ"
    );
    let denied = cop::document(f.backend.query("conventions.read", &cop::query(&*f.backend,"conventions.read",json!({"scope":{"kind":"Workspace"},"conventionId":owner.id,"format":"SectionPath","sectionPath":hit.indexed_path}))));
    match denied {
        Err(ops::Failure::Refused(fault)) => assert_eq!(
            fault.code,
            devworks_core::protocol::DomainErrorCode::NotFound
        ),
        _ => panic!("a captured project hit cannot override the current requested read scope"),
    }
    assert!(f.engine.host().user().project_id().is_none());
}

fn wait_index(f: &Fixture, owner: &ConventionRecord) {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let row = f.document(owner, "Markdown").convention;
        if matches!(
            row.index.status,
            ConventionIndexPhase::Ready | ConventionIndexPhase::LexicalOnly
        ) {
            return;
        }
        assert!(
            !matches!(row.index.status, ConventionIndexPhase::Failed),
            "{:?}",
            row.index
        );
        assert!(Instant::now() < until, "native body index: {:?}", row.index);
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn native_project_bulk_index_uses_exact_source_revision_after_listing_and_search() {
    let _native_case = native_runtime_case();
    use devworks_desktop::conventions_screen::Scope;
    use eframe::egui::accesskit::Role;
    let _case = common::native_runtime_case();
    let f = Fixture::new("convention-exact-source-revision");
    let owner = f.import(false);
    wait_index(&f, &owner);
    let mut shell = common::shell(&f.engine, &f.root, common::Script::default());
    shell.conventions.scope = Scope::Project(f.project.0.as_str().into());
    let mut ui = common::harness(shell, 1600.0, 1100.0);
    common::settle(&mut ui);
    common::click_nav(&mut ui, "Knowledge");
    common::click(&mut ui, Role::Button, "Conventions");
    common::step_until(&mut ui, "project source revision", |s| {
        s.conventions
            .state()
            .is_some_and(|v| v.loaded && v.source_scope_revision.is_some())
    });
    let state = ui.state().conventions.state().unwrap();
    assert_ne!(
        state.scope_revision, state.source_scope_revision,
        "inherited pagination revision differs from exact project source"
    );
    for searched in [false, true] {
        if searched {
            ui.state_mut().conventions.state_mut().text = "scopeguard".into();
            common::click(&mut ui, Role::Button, "Search conventions");
            common::step_until(&mut ui, "actual project lexical search", |s| {
                s.conventions
                    .state()
                    .is_some_and(|v| v.searched && !v.listing)
            });
        }
        let before = ui.state().conventions.state().unwrap().operation_ids.len();
        common::click(&mut ui, Role::Button, "Index this scope");
        common::step_until(&mut ui, "accepted exact project bulk index", |s| {
            s.conventions.state().unwrap().operation_ids.len() > before
        });
        let operation = ui
            .state()
            .conventions
            .state()
            .unwrap()
            .operation_ids
            .last()
            .unwrap()
            .clone();
        common::step_until(&mut ui, "published exact project bulk index", |s| {
            s.conventions
                .state()
                .unwrap()
                .operation_statuses
                .get(&operation)
                .is_some_and(|v| matches!(v.phase, ConventionOperationPhase::Completed))
        });
    }
}
#[test]
fn native_search_pages_large_legal_applicability_inside_real_response_frame() {
    let _native_case = native_runtime_case();
    let _case = common::native_runtime_case();
    let f = Fixture::new("convention-search-response-budget");
    let body = format!("# Budget\n{}", "budgetneedle source ".repeat(15_000));
    let labels = (0..128)
        .map(|n| format!("label-{n:03}-{}", "a".repeat(246)))
        .collect::<Vec<_>>();
    let owner = f.import_source(false, &body, &labels);
    wait_index(&f, &owner);
    let mut cursor = None;
    let mut ids = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let mut body = json!({"scope":f.scope(),"page":{"limit":50},"applicability":[],"text":"budgetneedle","topK":"50"});
        if let Some(cursor) = &cursor {
            body["page"]["cursor"] = json!(cursor);
        }
        let response = f.backend.query(
            "conventions.search",
            &cop::query(&*f.backend, "conventions.search", body),
        );
        let bytes =
            serde_json::to_vec(response.as_ref().expect("actual native search reply")).unwrap();
        assert!(
            bytes.len() <= devworks_core::framing::MAX_FRAME_BYTES,
            "{} byte native response",
            bytes.len()
        );
        let page = cop::search(response).unwrap();
        assert!(!page.items.0.is_empty());
        if pages == 0 {
            assert!(
                page.items.0.len() < 50,
                "byte budget must split the legal50-hit selection"
            );
        }
        for hit in page.items.0 {
            assert_eq!(hit.convention.applicability.0.len(), 128);
            assert!(
                ids.insert(hit.chunk_id.as_str().to_owned()),
                "continuation repeated a retained hit"
            );
        }
        pages += 1;
        assert!(pages < 10);
        assert_eq!(page.has_more, page.next_cursor.is_some());
        cursor = page.next_cursor.map(|v| v.0);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 50);
    assert!(pages > 1);
}
