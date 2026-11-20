//! Native gateway and SQLite exercise the document library before product cutover.
mod common;
#[path = "document_library/indexing.rs"]
mod indexing;
use common::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::{
    authority::{HostAuthority, service::bind_principal},
    context::{InvocationContext, PrincipalKind},
    gateway::Gateway,
    ports::{
        AuthorityPort,
        storage::{ApplicationStore, ArtifactScope, NewPrincipalBinding, StorageError},
    },
    protocol::{
        CommandReply, DomainErrorCode, ProjectId, Rejection,
        provider::ArtifactRef,
        records::{ArtifactReadContent, DocumentDetail, DocumentListResult, DocumentResult},
        results::{DomainDecision, DomainQueryReply, OperationResult},
    },
    services::{
        UseCases,
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        documents::DocumentLibrary,
    },
    settings::catalog::AgentRole,
};
use devworks_test_support::seed;
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture<'a> {
    h: &'a Harness,
    store: Arc<dyn ApplicationStore>,
    human: InvocationContext,
    artifacts: Arc<ArtifactService>,
    cases: UseCases,
}

impl<'a> Fixture<'a> {
    fn open(h: &'a Harness) -> Self {
        let store = h.open();
        let human = principal(store.as_ref(), "document-owner", PrincipalKind::Human);
        let files = Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("documents-cache"),
        ));
        let artifacts = Arc::new(ArtifactService::new(
            files,
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        ));
        let cases = UseCases::new()
            .with_artifacts(artifacts.clone())
            .with_document_library(Arc::new(DocumentLibrary::new(
                artifacts.clone(),
                h.ids.clone(),
            )));
        Self {
            h,
            store,
            human,
            artifacts,
            cases,
        }
    }

    fn scoped(&self, project: &str) -> InvocationContext {
        self.human
            .clone()
            .with_lineage(Some(ProjectId(id(project))), None, None)
    }

    fn source(&self, project: &str, bytes: &[u8]) -> ArtifactRef {
        self.typed_source(project, bytes, "text/markdown")
    }

    fn typed_source(&self, project: &str, bytes: &[u8], media_type: &'static str) -> ArtifactRef {
        let artifact = self.artifacts.clone();
        let staged = artifact
            .stage_host_bytes(self.store.as_ref(), bytes, json!({"kind":"FixtureUpload"}))
            .unwrap()
            .unwrap();
        let (owned, stage, owner, scope) = (
            artifact.clone(),
            staged.clone(),
            self.human.principal().clone(),
            ArtifactScope::Project(ProjectId(id(project))),
        );
        let version = write(self.store.as_ref(), move |tx| {
            owned.commit_host_bytes_in_scope(
                tx,
                &stage,
                HostArtifactPublication {
                    title: "Uploaded notes",
                    media_type,
                    scope,
                    owner,
                    provenance: json!({"kind":"FixtureUpload"}),
                },
            )
        })
        .unwrap();
        artifact.settle_host_bytes(self.store.as_ref(), &staged, true);
        version_ref(&version)
    }

    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        frame: Value,
    ) -> CommandReply<DomainDecision> {
        let authority = HostAuthority {
            store: self.store.as_ref(),
            clock: &*self.h.clock,
            protected_branches: &[],
        };
        self.command_with(ctx, operation, frame, &authority)
    }

    fn command_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        frame: Value,
        authority: &dyn AuthorityPort,
    ) -> CommandReply<DomainDecision> {
        let gateway = Gateway {
            store: self.store.as_ref(),
            ids: &*self.h.ids,
            authority,
        };
        self.cases
            .command(&gateway, ctx, operation, frame.to_string().as_bytes())
            .unwrap()
    }

    fn query(&self, ctx: &InvocationContext, operation: &str, frame: Value) -> DomainQueryReply {
        let authority = HostAuthority {
            store: self.store.as_ref(),
            clock: &*self.h.clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            store: self.store.as_ref(),
            ids: &*self.h.ids,
            authority: &authority,
        };
        self.cases
            .query(&gateway, ctx, operation, frame.to_string().as_bytes())
            .unwrap()
    }

    fn import(
        &self,
        ctx: &InvocationContext,
        key: &str,
        source: &ArtifactRef,
        old: Option<&DocumentDetail>,
    ) -> CommandReply<DomainDecision> {
        let mut body = json!({"kind":"documents.import","scope":{"kind":"Project","projectId":ctx.project_id()},
            "source":source,"title":"Notes"});
        if let Some(old) = old {
            body["replaceDocumentId"] = json!(old.id);
            body["expectedRevision"] = json!(old.revision);
        }
        self.command(ctx, "documents.import",
            json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":10,"requestKey":key,"body":body}))
    }

    fn read_ref(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        reference: &ArtifactRef,
        revision: u32,
    ) -> DomainQueryReply {
        self.query(ctx, operation, json!({
            "protocol":"1.0-draft.1","schemaRevision":revision,"queryId":"read-document","limit":20,
            "query":{"kind":"Read","artifactId":reference.artifact_id,"versionId":reference.version_id}
        }))
    }

    fn delete(&self, reference: &ArtifactRef, key: &str) {
        let before = read(self.store.as_ref(), |read| {
            read.artifacts().artifact(&reference.artifact_id)
        })
        .unwrap();
        let result = applied(self.command(&self.human, "artifacts.delete", json!({
            "protocol":"1.0-draft.1","requestKey":key,"deletion":{"kind":"DeleteVersion",
                "artifactId":reference.artifact_id,"versionId":reference.version_id,
                "expectedRevision":before.revision.to_string(),"expectedReferenceRevision":before.reference_revision.to_string()}
        })));
        assert!(matches!(result, OperationResult::ArtifactsDelete { .. }));
    }
}

#[track_caller]
fn applied(reply: CommandReply<DomainDecision>) -> OperationResult {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => result,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[track_caller]
fn result(reply: DomainQueryReply) -> OperationResult {
    match reply {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("{other:?}"),
    }
}

#[test]
fn native_document_versions_share_move_and_survive_restart() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let (retained, replaced) = {
        let f = Fixture::open(&h);
        seed::rows(
            f.store.as_ref(),
            &[
                seed::ProjectSeed::new("document-a").name("A"),
                seed::ProjectSeed::new("document-b").name("B"),
                seed::ProjectSeed::new("document-c").name("C"),
            ],
            &[],
        )
        .unwrap();
        let a = f.scoped("document-a");
        let b = f.scoped("document-b");
        let c = f.scoped("document-c");
        let source = f.source("document-a", b"# Original\nVersion one");
        let mut wrong_type = source.clone();
        wrong_type.media_type = "application/pdf".into();
        assert!(
            matches!(f.import(&a, "wrong-artifact-media-type", &wrong_type, None),
            CommandReply::Recorded { receipt, .. } if matches!(&receipt.decision,
                DomainDecision::Refused { fault, .. } if fault.code == DomainErrorCode::NotFound))
        );
        let first = applied(f.import(&a, "import-original", &source, None));
        let OperationResult::DocumentsImport {
            value: DocumentResult::Current(first),
        } = first
        else {
            panic!("{first:?}")
        };
        assert_ne!(first.id, source.artifact_id);
        let retained = first.current_version.clone().unwrap();
        assert_eq!(retained.sha256, source.sha256);
        let imported = read(f.store.as_ref(), |r| {
            r.artifacts().version(&retained.version_id)
        })
        .unwrap();
        assert_eq!(imported.source["importedFrom"], json!(source));
        let replay = applied(f.import(&a, "import-original", &source, None));
        assert!(
            matches!(replay, OperationResult::DocumentsImport { value: DocumentResult::Current(value) }
            if value.id == first.id && value.retained_version_count.get() == 1)
        );
        f.delete(&source, "delete-upload");
        let old_input = f.source("document-a", b"# Edited\nVersion two");
        let second = applied(f.import(&a, "import-replacement", &old_input, Some(&first)));
        let OperationResult::DocumentsImport {
            value: DocumentResult::Current(second),
        } = second
        else {
            panic!("{second:?}")
        };
        assert_eq!(second.id, first.id);
        assert_eq!(second.retained_version_count.get(), 2);
        let replaced = second.current_version.clone().unwrap();
        assert_ne!(replaced.sha256, retained.sha256);
        assert!(matches!(f.read_ref(&b, "documents.read", &retained, 10),
            DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::NotFound));
        let share = json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"share-document",
            "command":{"kind":"Share","artifactId":second.id,"targetProjectId":"document-b","expectedScopeRevision":second.scope_revision}});
        let wrong = f.command(&f.human, "documents.move", share.clone());
        assert!(
            matches!(
                wrong,
                CommandReply::Rejected {
                    code: Rejection::InvalidRequest,
                    ..
                }
            ),
            "{wrong:?}"
        );
        let share = applied(f.command(&f.human, "documents.share", share));
        let OperationResult::DocumentsShare {
            value: DocumentResult::Current(shared),
        } = share
        else {
            panic!("{share:?}")
        };
        assert_eq!(shared.scope_revision.get(), second.scope_revision.get() + 1);
        let mut page = json!({"protocol":"1.0-draft.1","schemaRevision":10,
            "queryId":"try-version-page","limit":1,"query":{"kind":"Read",
                "artifactId":retained.artifact_id,"versionId":retained.version_id}});
        let first_page = result(f.query(&b, "documents.read", page.clone()));
        let OperationResult::DocumentsRead {
            value: DocumentResult::Current(first_page),
        } = first_page
        else {
            panic!("{first_page:?}")
        };
        assert_eq!(
            first_page.retained_versions.0[0].version_id,
            replaced.version_id
        );
        page["cursor"] = json!(first_page.next_version_cursor.unwrap());
        let next = result(f.query(&b, "documents.read", page.clone()));
        assert!(
            matches!(next, OperationResult::DocumentsRead { value: DocumentResult::Current(value) }
            if value.next_version_cursor.is_none() && value.retained_versions.0[0].version_id == retained.version_id)
        );
        let as_project = |name| ProjectId(id(name));
        let inventory = |project| {
            read(f.store.as_ref(), |r| {
                r.projects().purge_inventory(&as_project(project))
            })
            .unwrap()
        };
        assert!(
            !inventory("document-a")
                .client
                .deleted_artifact_ids
                .contains(&retained.artifact_id)
        );

        let move_frame = |revision, key| {
            json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":key,
            "command":{"kind":"Move","artifactId":second.id,"targetProjectId":"document-c","expectedScopeRevision":revision}})
        };
        let stale = f.command(
            &f.human,
            "documents.move",
            move_frame(second.scope_revision, "stale-document-move"),
        );
        assert!(
            matches!(&stale, CommandReply::Recorded { receipt, .. } if matches!(&receipt.decision,
            DomainDecision::Refused { fault, .. } if fault.code == DomainErrorCode::RevisionConflict
                && fault.expected_revision.as_ref().is_some_and(|revision| revision.get() == 1)
                && fault.current_revision.as_ref().is_some_and(|revision| revision.get() == 2))),
            "{stale:?}"
        );
        assert_eq!(
            read(f.store.as_ref(), |r| r.artifacts().artifact(&second.id))
                .unwrap()
                .scope_revision,
            shared.scope_revision.get()
        );
        let moved = applied(f.command(
            &f.human,
            "documents.move",
            move_frame(shared.scope_revision, "move-document"),
        ));
        assert!(
            matches!(moved, OperationResult::DocumentsMove { value: DocumentResult::Current(value) }
            if value.id == second.id && value.scope_revision.get() == 3)
        );
        assert!(matches!(f.query(&b, "documents.read", page),
            DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::CursorExpired));
        assert!(matches!(f.read_ref(&a, "documents.read", &retained, 10),
            DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::NotFound));
        for other in [&b, &c] {
            assert!(
                matches!(result(f.read_ref(other, "documents.read", &retained, 10)), OperationResult::DocumentsRead {
                value: DocumentResult::Current(value)
            } if value.selected_version.as_ref().is_some_and(|ref_| ref_.version_id == retained.version_id)
                && value.current_version.as_ref().is_some_and(|ref_| ref_.version_id == replaced.version_id))
            );
        }
        let stored = read(f.store.as_ref(), |r| r.artifacts().artifact(&second.id)).unwrap();
        assert_eq!(stored.origin_project_id, Some(as_project("document-a")));
        let mut missing = retained.clone();
        missing.version_id = id("unpublished-document-version");
        assert!(matches!(f.read_ref(&c, "documents.read", &missing, 10),
            DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::ViewUnavailable));
        assert_eq!(
            stored.scope,
            ArtifactScope::Project(as_project("document-c"))
        );
        assert!(
            !inventory("document-a")
                .client
                .deleted_artifact_ids
                .contains(&retained.artifact_id)
        );
        assert!(matches!(
            result(f.read_ref(&c, "documents.read", &retained, 1)),
            OperationResult::DocumentsRead {
                value: DocumentResult::Legacy(_)
            }
        ));
        (retained, replaced)
    };
    h.wait_released();
    let f = Fixture::open(&h);
    let b = f.scoped("document-b");
    let read_bytes = result(f.read_ref(&b, "artifacts.read", &retained, 1));
    assert!(
        matches!(read_bytes, OperationResult::ArtifactsRead { value: ArtifactReadContent::Text { content, .. } }
        if content.0 == "# Original\nVersion one")
    );

    f.delete(&replaced, "delete-document-current");
    let latest = result(f.read_ref(&b, "documents.read", &replaced, 10));
    assert!(
        matches!(latest, OperationResult::DocumentsRead { value: DocumentResult::Current(value) }
        if value.current_version.is_none() && value.selected_version.is_none()
        && value.deleted_current_version_id == Some(replaced.version_id.clone())
        && value.retained_versions.0[0].version_id == retained.version_id)
    );
    assert!(matches!(f.read_ref(&b, "documents.read", &retained, 1),
        DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::ViewUnavailable));
    let listed = result(f.query(&b, "documents.list", json!({
        "protocol":"1.0-draft.1","mode":"Query","schemaRevision":10,"queryId":"list-disabled-docs",
        "body":{"kind":"documents.list","scope":{"kind":"Project","projectId":"document-b"},
            "retrievalEnabled":false,"page":{"limit":10}}
    })));
    assert!(
        matches!(listed, OperationResult::DocumentsList { value: DocumentListResult::Current(value) }
        if value.items.0.len() == 1 && value.items.0[0].id == retained.artifact_id
            && value.items.0[0].current_version.is_none())
    );
    f.delete(&retained, "delete-last-retained");
    let c = f.scoped("document-c");
    let tombstone = result(f.read_ref(&c, "documents.read", &retained, 10));
    assert!(
        matches!(tombstone, OperationResult::DocumentsRead { value: DocumentResult::Current(value) }
        if value.current_version.is_none() && value.selected_version.is_none()
            && value.deleted_selected_version_id == Some(retained.version_id.clone())
            && value.retained_version_count.get() == 0)
    );
    assert!(matches!(f.read_ref(&b, "documents.read", &retained, 10),
        DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::NotFound));
}

#[test]
fn native_document_import_and_cursors_bind_the_selected_format_scope_and_revision() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let f = Fixture::open(&h);
    seed::rows(
        f.store.as_ref(),
        &[
            seed::ProjectSeed::new("workspace").name("A project named workspace"),
            seed::ProjectSeed::new("other-doc-project").name("Another project"),
        ],
        &[],
    )
    .unwrap();
    let ctx = f.scoped("workspace");
    let image = f.typed_source("workspace", b"unrelated artifact", "image/png");
    assert!(
        matches!(f.import(&ctx, "unsupported-doc-source", &image, None),
        CommandReply::Recorded { receipt, .. } if matches!(&receipt.decision,
            DomainDecision::Refused { fault, .. } if fault.code == DomainErrorCode::UnsupportedTarget))
    );
    let text = f.source("workspace", b"importable plain Markdown");
    let first = applied(f.import(&ctx, "first-document", &text, None));
    let OperationResult::DocumentsImport {
        value: DocumentResult::Current(first),
    } = first
    else {
        panic!("{first:?}")
    };
    let original = first.current_version.unwrap();
    assert!(matches!(
        applied(f.import(&ctx, "second-document", &text, None)),
        OperationResult::DocumentsImport { .. }
    ));
    let frame = |scope, revision, cursor: Option<Value>| {
        let mut page = json!({"limit":1});
        if let Some(cursor) = cursor {
            page["cursor"] = cursor;
        }
        json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":revision,
            "queryId":"native-list-binding","body":{"kind":"documents.list","scope":scope,"page":page}})
    };
    let project = json!({"kind":"Project","projectId":"workspace"});
    let first = result(f.query(&f.human, "documents.list", frame(project.clone(), 1, None)));
    let OperationResult::DocumentsList {
        value: DocumentListResult::Legacy(first),
    } = first
    else {
        panic!("{first:?}")
    };
    let cursor = json!(first.next_cursor.unwrap());
    for next in [
        frame(json!({"kind":"Workspace"}), 1, Some(cursor.clone())),
        frame(project, 10, Some(cursor)),
    ] {
        assert!(matches!(
            f.query(&f.human, "documents.list", next),
            DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::CursorExpired
        ));
    }
    // Populate longer history using the native version repository. The real gateway must
    // retain the explicitly requested oldest ref in a bounded legacy result.
    let oldest_id = original.version_id.clone();
    let current = write(f.store.as_ref(), move |tx| {
        let oldest = tx.artifacts().version(&oldest_id)?.unwrap();
        let mut artifact = tx.artifacts().artifact(&oldest.artifact_id)?.unwrap();
        let mut current = oldest;
        for version_no in 2..=501 {
            current.id = id(&format!("document-stored-history-{version_no}"));
            current.version_no = version_no;
            tx.artifacts().insert_version(&current)?;
        }
        artifact.current_version_id = Some(current.id.clone());
        artifact.revision = current.version_no;
        tx.artifacts().update_artifact(&artifact)?;
        Ok(current.id)
    })
    .unwrap();
    let read_old = result(f.read_ref(&ctx, "documents.read", &original, 1));
    assert!(
        matches!(read_old, OperationResult::DocumentsRead { value: DocumentResult::Legacy(value) }
        if value.current_version.version_id == current && value.retained_versions.0.len() == 500
            && value.retained_versions.0.iter().any(|version| version.version_id == original.version_id))
    );

    let worker = principal(
        f.store.as_ref(),
        "confined-document-reader",
        PrincipalKind::Agent,
    )
    .with_lineage(Some(ProjectId(id("workspace"))), None, None);
    let binder = f.human.clone();
    let actor = worker.principal().clone();
    write(f.store.as_ref(), move |tx| {
        bind_principal(
            tx,
            &binder,
            NewPrincipalBinding {
                actor,
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: Some(ProjectId(id("workspace"))),
                tool_families: Some(vec!["documents".into()]),
                bound_by: binder.principal().clone(),
            },
            None,
        )
        .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
        Ok(())
    })
    .unwrap();
    // Admit the selected handler directly so this case reaches the library's own scope fence.
    for target in ["other-doc-project", "missing-doc-project"] {
        let response = f.command_with(&worker, "documents.share",
            json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":format!("share-{target}"),
                "command":{"kind":"Share","artifactId":original.artifact_id,
                    "targetProjectId":target,"expectedScopeRevision":"1"}}), &Authority);
        assert!(
            matches!(&response, CommandReply::Recorded { receipt, .. }
            if matches!(&receipt.decision, DomainDecision::Refused { fault, .. }
                if fault.code == DomainErrorCode::PermissionDenied)),
            "{response:?}"
        );
    }
}
