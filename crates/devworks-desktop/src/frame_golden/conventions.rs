//! Sample frames of the convention manager's requests: listings, searches, source reads,
//! version and resource pages, operation status reads, the direct commands and the prepared
//! edit, import and resource pipelines.
use super::{Silent, from};
use crate::conventions_ops as cop;
use crate::conventions_screen::{Draft, Scope, Target};
use crate::conventions_transfer as transfer;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{ConventionRecord, ConventionResourceRecord};
use serde_json::json;

fn artifact(id: &str) -> ArtifactRef {
    from(
        json!({"artifactId":id,"versionId":"version-a","sha256":"a".repeat(64),
        "byteLength":"12","mediaType":"text/markdown"}),
    )
}

fn record(with_raw: bool, retired: bool) -> ConventionRecord {
    let mut v = json!({
        "id": "convention-a", "revision": "7", "scope": {"kind": "Workspace"},
        "slug": "naming", "title": "Naming", "parentId": "convention-root", "sortOrder": "-3",
        "lifecycle": if retired { "Retired" } else { "Active" },
        "content": artifact("content-a"), "applicability": ["rust", "tests"],
        "provenance": {"kind": "Authored", "actorId": "actor-a"}, "authorId": "actor-a",
        "createdAtMs": "1", "updatedAtMs": "2", "legacyApproved": false,
        "index": {"status": "NotIndexed"}});
    if with_raw {
        v["rawContent"] = json!(artifact("raw-a"));
    }
    from(v)
}

fn resource() -> ConventionResourceRecord {
    from(
        json!({"id": "resource-a", "revision": "3", "conventionId": "convention-a",
        "conventionRevision": "7", "scope": {"kind": "Workspace"}, "name": "Diagram",
        "lifecycle": "Active", "content": artifact("resource-content"),
        "applicability": [], "authorId": "actor-a", "createdAtMs": "1",
        "mediaType": "image/png"}),
    )
}

fn target(id: &str) -> Target {
    Target {
        id: id.into(),
        by_slug: false,
        path: None,
        revision: None,
        format: 0,
        section: None,
    }
}

fn submission(scope: &Scope, operation: &str) -> cop::Submission {
    cop::Submission {
        scope: scope.clone(),
        operation: operation.into(),
        target: None,
        draft: None,
        reader_generation: 0,
        resource: None,
    }
}

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let workspace = Scope::Workspace;
    let project = Scope::Project("project-x".into());
    let applicability = vec!["rust".to_owned(), "tests".to_owned()];
    let mut out = Vec::new();

    // Listings and searches: the first page is built from the screen's filters, a later page
    // is the first page's request with the continuation cursor.
    let list = cop::list(&workspace, &applicability, 2, 1).unwrap();
    out.push(("conventions_list_workspace", cop::query(b, list.clone())));
    out.push((
        "conventions_list_more",
        cop::query(b, cop::at_cursor(&list, "cursor-1")),
    ));
    out.push((
        "conventions_list_project_deleted",
        cop::query(b, cop::list(&project, &applicability, 3, 0)),
    ));
    out.push((
        "conventions_search_top_k",
        cop::query(
            b,
            cop::search_input(&project, &applicability, "naming rules", "5"),
        ),
    ));
    let search = cop::search_input(&project, &applicability, "naming rules", "").unwrap();
    out.push(("conventions_search", cop::query(b, search.clone())));
    out.push((
        "conventions_search_more",
        cop::query(b, cop::at_cursor(&search, "cursor-2")),
    ));
    out.push((
        "conventions_parents_first",
        cop::query(b, cop::parents(&project, None)),
    ));
    out.push((
        "conventions_parents_more",
        cop::query(b, cop::parents(&project, Some("cursor-3"))),
    ));

    // Source reads of the reader pane.
    let by_id = Target {
        revision: Some(7),
        format: 1,
        ..target("convention-a")
    };
    let by_slug = Target {
        by_slug: true,
        format: 2,
        ..target("naming")
    };
    let by_path = Target {
        path: Some(vec!["Rules".into(), "Naming".into()]),
        ..target("ignored")
    };
    let section = Target {
        format: 3,
        section: Some(from(
            json!({"generationId": "generation-a", "chunkId": "chunk-a"}),
        )),
        ..target("convention-a")
    };
    for (name, target) in [
        ("conventions_read_id", &by_id),
        ("conventions_read_slug", &by_slug),
        ("conventions_read_path", &by_path),
        ("conventions_read_section", &section),
    ] {
        out.push((name, cop::query(b, cop::read(&workspace, target))));
    }
    let intent = cop::read(&project, &by_id).unwrap();
    out.push((
        "conventions_read_cursor",
        cop::query(b, cop::at_cursor(&intent, "cursor-4")),
    ));
    out.push((
        "conventions_read_operation",
        cop::query(b, cop::operation_status(&project, "operation-a")),
    ));
    let r = record(true, false);
    out.push((
        "conventions_source_markdown",
        cop::query(
            b,
            cop::read_source(&workspace, &r.id, &r.revision, None, "Markdown", None),
        ),
    ));
    out.push((
        "conventions_source_raw_cursor",
        cop::query(
            b,
            cop::read_source(&project, &r.id, &r.revision, None, "Raw", Some("cursor-5")),
        ),
    ));
    out.push((
        "conventions_source_manifest",
        cop::query(
            b,
            cop::read_source(&workspace, &r.id, &r.revision, None, "Manifest", None),
        ),
    ));
    let res = resource();
    let download = |cursor| {
        cop::read_source(
            &project,
            &res.convention_id,
            &res.convention_revision,
            Some(&res.id),
            "Resource",
            cursor,
        )
    };
    out.push(("conventions_download_first", cop::query(b, download(None))));
    out.push((
        "conventions_download_cursor",
        cop::query(b, download(Some("cursor-6"))),
    ));

    // Versions and resources.
    out.push((
        "conventions_versions_first",
        cop::query(b, cop::versions(&workspace, &r.id, None)),
    ));
    out.push((
        "conventions_versions_more",
        cop::query(b, cop::versions(&workspace, &r.id, Some("cursor-7"))),
    ));
    let resources =
        |mode| cop::resources_input(&workspace, mode, &applicability, Some(&r), "handbook");
    out.push((
        "conventions_resources_attachments",
        cop::query(b, resources(0)),
    ));
    let catalog = resources(1).unwrap();
    out.push((
        "conventions_resources_catalog",
        cop::query(b, catalog.clone()),
    ));
    out.push((
        "conventions_resources_more",
        cop::query(b, cop::at_cursor(&catalog, "cursor-8")),
    ));
    let document = resources(2).unwrap();
    out.push((
        "conventions_resources_document",
        cop::query(b, document.clone()),
    ));
    out.push((
        "conventions_resources_document_cursor",
        cop::query(b, cop::at_document_cursor(&document, Some("cursor-9"))),
    ));

    // Direct commands.
    out.push((
        "conventions_applicability_retired",
        cop::command(b, cop::applicability(&workspace, &r, &applicability, true)),
    ));
    out.push((
        "conventions_applicability_active",
        cop::command(b, cop::applicability(&project, &r, &[], false)),
    ));
    out.push((
        "conventions_retire",
        cop::command(b, cop::retire(&workspace, &r).1),
    ));
    out.push((
        "conventions_reactivate",
        cop::command(b, cop::retire(&workspace, &record(false, true)).1),
    ));
    out.push((
        "conventions_cancel_operation",
        cop::command(b, cop::cancel_operation(&project, "operation-a", 9)),
    ));
    out.push((
        "conventions_delete",
        cop::command(b, cop::delete(&workspace, &r)),
    ));
    out.push((
        "conventions_delete_resource",
        cop::command(b, cop::delete_resource(&project, &res)),
    ));
    out.push((
        "conventions_move_root",
        cop::command(b, cop::move_to(&workspace, &r, &project, None, -4)),
    ));
    out.push((
        "conventions_move_child",
        cop::command(
            b,
            cop::move_to(&workspace, &r, &project, Some("convention-b"), -4),
        ),
    ));
    out.push((
        "conventions_index_all",
        cop::command(b, cop::index_scope(&project, 11)),
    ));
    out.push((
        "conventions_index_one",
        cop::command(b, cop::index(&workspace, &r)),
    ));

    // Prepared pipelines: the staged request with each uploaded artifact put in place.
    let submitted = |p: &transfer::Pipeline| cop::command(b, p.body.input());
    let uploaded = artifact("uploaded");
    let open = |raw: bool| {
        let mut d = Draft::of(&r);
        d.body_complete = true;
        d.loading = false;
        d.raw_mode = usize::from(raw);
        d
    };
    let plain = transfer::prepare_edit(
        b,
        workspace.clone(),
        open(false),
        submission(&workspace, "conventions.edit"),
    )
    .expect("edit");
    out.push(("conventions_pipeline_edit", submitted(&plain)));
    let mut changed = open(true);
    changed.markdown_changed = true;
    changed.raw_changed = true;
    changed.markdown = "# New".to_owned().into();
    changed.raw = "raw".to_owned().into();
    let mut staged = transfer::prepare_edit(
        b,
        project.clone(),
        changed,
        submission(&project, "conventions.edit"),
    )
    .expect("edit");
    out.push(("conventions_pipeline_edit_staged", submitted(&staged)));
    transfer::apply_artifact(&mut staged, uploaded.clone()).expect("content");
    transfer::apply_artifact(&mut staged, artifact("uploaded-raw")).expect("raw");
    out.push(("conventions_pipeline_edit_uploaded", submitted(&staged)));
    let kept = transfer::prepare_edit(
        b,
        workspace.clone(),
        open(true),
        submission(&workspace, "conventions.edit"),
    )
    .expect("edit");
    out.push(("conventions_pipeline_edit_raw_kept", submitted(&kept)));
    let mut removed = open(false);
    removed.base = Some(record(true, false));
    removed.raw_mode = 2;
    let removed = transfer::prepare_edit(
        b,
        workspace.clone(),
        removed,
        submission(&workspace, "conventions.edit"),
    )
    .expect("edit");
    out.push(("conventions_pipeline_edit_raw_removed", submitted(&removed)));
    let fresh = Draft {
        title: "Fresh".into(),
        slug: "fresh".into(),
        markdown: "# Fresh".to_owned().into(),
        ..Default::default()
    };
    let mut imported = transfer::prepare_edit(
        b,
        project.clone(),
        fresh,
        submission(&project, "conventions.import"),
    )
    .expect("import");
    transfer::apply_artifact(&mut imported, uploaded.clone()).expect("source");
    out.push(("conventions_pipeline_import_uploaded", submitted(&imported)));

    // Resource attach and update.
    for (name, update) in [
        ("conventions_resource_attach", false),
        ("conventions_resource_update", true),
    ] {
        let content = r.content.clone().expect("content");
        let change = if update {
            from(
                json!({"kind":"Update","resourceId":res.id,"expectedRevision":res.revision,"name":"Renamed","mediaType":"image/jpeg","content":res.content,"lifecycle":"Retired"}),
            )
        } else {
            from(json!({"kind":"Attach","name":"Upload","mediaType":"image/png","content":content}))
        };
        let mut p = transfer::Pipeline {
            scope: workspace.clone(),
            operation: "conventions.edit".into(),
            body: transfer::resource_edit(&workspace, &r, &content, change).expect("resource edit"),
            files: vec![transfer::StagedFile {
                path: "/staged/upload".into(),
                name: "Upload".into(),
                media_type: "image/png".into(),
                pointer: "/resources/0/content".into(),
                owned: false,
            }],
            at: 0,
            submission: submission(&workspace, "conventions.edit"),
        };
        out.push((name, submitted(&p)));
        transfer::apply_artifact(&mut p, uploaded.clone()).expect("resource");
        let key: &'static str = Box::leak(format!("{name}_uploaded").into_boxed_str());
        out.push((key, submitted(&p)));
    }
    out
}
