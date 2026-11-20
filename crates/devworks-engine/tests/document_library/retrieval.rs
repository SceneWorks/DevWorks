use super::{
    indexing::{Processor, drive, imported_result, index_frame, indexed, management_frame, run_id},
    *,
};
use devworks_core::{
    protocol::{
        data::{Citation, QueryCompletion, RetrievalMode, RetrievalPage},
        records::ResolvedDocumentCitation,
    },
    services::documents::indexing::DocumentIndexer,
};

fn compose<'a>(harness: &'a Harness) -> (Fixture<'a>, Arc<DocumentIndexer>) {
    let mut fixture = Fixture::open(harness);
    seed::rows(
        fixture.store.as_ref(),
        &[
            seed::ProjectSeed::new("document-a"),
            seed::ProjectSeed::new("document-b"),
        ],
        &[],
    )
    .unwrap();
    let store = fixture.store.clone();
    let indexer = indexed(&mut fixture, store, Processor::new());
    (fixture, indexer)
}

fn put(
    f: &Fixture<'_>,
    indexer: &Arc<DocumentIndexer>,
    ctx: &InvocationContext,
    key: &str,
    bytes: &[u8],
    replaces: Option<&DocumentDetail>,
) -> DocumentDetail {
    let project = ctx.project_id().unwrap().0.as_str();
    let uploaded = f.source(project, bytes);
    let record = imported_result(f.import(ctx, key, &uploaded, replaces));
    run_id(f.command(
        ctx,
        "documents.index",
        index_frame("documents.index", &format!("{key}-index"), &record),
    ));
    drive(f, indexer.clone());
    record
}

fn envelope(
    ctx: &InvocationContext,
    text: &str,
    exact: &[&ArtifactRef],
    result_limit: u16,
    visited: u32,
) -> Value {
    json!({
        "protocol":"1.0-draft.1","queryId":"native-search",
        "scope":{"kind":"Project","projectId":ctx.project_id()},
        "sources":["Document"],
        "versions": if exact.is_empty() { json!({"kind":"Current"}) } else { json!({"kind":"Exact","versions":exact}) },
        "input":{"kind":"Text","text":text},"mode":"Lexical","lexicalFallback":false,
        "limits":{"maxResults":result_limit,"maxVisited":visited,"deadlineMs":2000}
    })
}

fn search(
    f: &Fixture<'_>,
    ctx: &InvocationContext,
    text: &str,
    exact: &[&ArtifactRef],
) -> RetrievalPage {
    searched(f.query(
        ctx,
        "documents.search",
        envelope(ctx, text, exact, 50, 1000),
    ))
}

fn searched(reply: DomainQueryReply) -> RetrievalPage {
    let OperationResult::DocumentsSearch { value } = result(reply) else {
        panic!("expected native search")
    };
    value
}

fn cite_envelope(citation: &Citation, revision: u32) -> Value {
    json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":revision,"queryId":"resolve",
        "body":{"kind":"documents.resolve_citation","citation":citation}})
}

fn open(f: &Fixture<'_>, ctx: &InvocationContext, citation: &Citation) -> ResolvedDocumentCitation {
    let OperationResult::DocumentsResolveCitation { value } = result(f.query(
        ctx,
        "documents.resolve_citation",
        cite_envelope(citation, 11),
    )) else {
        panic!("expected exact citation")
    };
    value
}

fn assert_unavailable(reply: DomainQueryReply) {
    assert!(
        matches!(&reply, DomainQueryReply::Rejected { fault, .. }
        if fault.code == DomainErrorCode::ViewUnavailable),
        "{reply:?}"
    );
}

#[test]
fn current_and_exact_retrieval_follow_shares_replacements_moves_and_deletion() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let (f, indexer) = compose(&h);
    let a = f.scoped("document-a");
    let b = f.scoped("document-b");
    let old = put(
        &f,
        &indexer,
        &a,
        "old",
        "Earlier résumé passage.".as_bytes(),
        None,
    );
    let old_ref = old.current_version.as_ref().unwrap();
    let before = search(&f, &a, "resume", &[]);
    assert_eq!(before.passages.len(), 1);
    assert_eq!(before.passages[0].rank, 1);
    assert_eq!(before.generation_ids.len(), 1);
    let citation = before.passages[0].citation.clone();
    assert!(search(&f, &b, "resume", &[]).passages.is_empty());
    assert_unavailable(f.query(
        &b,
        "documents.search",
        envelope(&b, "resume", &[old_ref], 50, 1000),
    ));
    assert_unavailable(f.query(
        &b,
        "documents.resolve_citation",
        cite_envelope(&citation, 11),
    ));
    let OperationResult::DocumentsShare { value: DocumentResult::Current(shared) } = applied(f.command(
        &f.human, "documents.share", json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"share-old",
            "command":{"kind":"Share","artifactId":old.id,"targetProjectId":"document-b","expectedScopeRevision":old.scope_revision}})
    )) else { panic!("expected shared document") };
    assert_eq!(search(&f, &b, "resume", &[old_ref]).passages.len(), 1);
    assert_eq!(open(&f, &b, &citation).text.0, "Earlier résumé passage.");

    let replacement = f.source("document-a", b"Current document passage.");
    let current = imported_result(f.import(&a, "new", &replacement, Some(&shared)));
    let current_ref = current.current_version.as_ref().unwrap();
    assert!(search(&f, &a, "resume", &[]).passages.is_empty());
    assert!(search(&f, &a, "current", &[]).passages.is_empty());
    run_id(f.command(
        &a,
        "documents.index",
        index_frame("documents.index", "new-index", &current),
    ));
    drive(&f, indexer.clone());
    assert_eq!(
        search(&f, &a, "resume", &[old_ref]).passages[0].text,
        "Earlier résumé passage."
    );
    assert_eq!(search(&f, &b, "current", &[]).passages.len(), 1);
    let mut bad_ref = old_ref.clone();
    bad_ref.media_type = "text/plain".into();
    assert_unavailable(f.query(
        &b,
        "documents.search",
        envelope(&b, "current", &[current_ref, &bad_ref], 50, 1000),
    ));
    let OperationResult::DocumentsMove { value: DocumentResult::Current(moved) } = applied(f.command(
        &f.human, "documents.move", json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"move-current",
            "command":{"kind":"Move","artifactId":current.id,"targetProjectId":"document-b","expectedScopeRevision":current.scope_revision}})
    )) else { panic!("expected moved document") };
    assert!(search(&f, &a, "current", &[]).passages.is_empty());
    assert_unavailable(f.query(
        &a,
        "documents.resolve_citation",
        cite_envelope(&citation, 11),
    ));
    assert_eq!(search(&f, &b, "current", &[]).passages.len(), 1);
    let OperationResult::DocumentsDisableRetrieval {
        value: DocumentResult::Current(disabled),
    } = applied(f.command(
        &b,
        "documents.disable_retrieval",
        management_frame("DisableRetrieval", "disable-index", &moved),
    ))
    else {
        panic!("expected disabled retrieval")
    };
    assert!(search(&f, &b, "current", &[]).passages.is_empty());
    assert_unavailable(f.query(
        &b,
        "documents.search",
        envelope(&b, "resume", &[old_ref], 50, 1000),
    ));
    assert_eq!(open(&f, &b, &citation).text.0, "Earlier résumé passage.");
    assert!(matches!(
        applied(f.command(
            &b,
            "documents.remove_index",
            management_frame("RemoveIndex", "remove-index", &disabled)
        )),
        OperationResult::DocumentsRemoveIndex { .. }
    ));
    assert_eq!(open(&f, &b, &citation).text.0, "Earlier résumé passage.");
    f.delete(old_ref, "delete-indexed-old-version");
    assert_unavailable(f.query(
        &b,
        "documents.resolve_citation",
        cite_envelope(&citation, 11),
    ));
}

#[test]
fn relevance_work_and_result_limits_use_only_eligible_postings() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let (f, indexer) = compose(&h);
    let a = f.scoped("document-a");
    let b = f.scoped("document-b");
    put(
        &f,
        &indexer,
        &a,
        "hidden",
        "needle apple banana ".repeat(1_000).as_bytes(),
        None,
    );
    put(&f, &indexer, &b, "weaker", b"needle apple", None);
    put(&f, &indexer, &b, "stronger", b"needle apple banana", None);
    let visible = searched(f.query(
        &b,
        "documents.search",
        envelope(&b, "needle apple banana", &[], 1, 6),
    ));
    assert_eq!(visible.passages.len(), 1);
    assert_eq!(visible.passages[0].text, "needle apple banana");
    assert!(matches!(visible.completion, QueryCompletion::ResultLimit));
    let partial = searched(f.query(
        &b,
        "documents.search",
        envelope(&b, "needle apple banana", &[], 10, 3),
    ));
    assert_eq!(partial.passages.len(), 1);
    assert!(matches!(partial.completion, QueryCompletion::WorkLimit));
    let mut fallback = envelope(&b, "needle apple banana", &[], 10, 6);
    fallback["mode"] = json!("Hybrid");
    let refused = f.query(&b, "documents.search", fallback.clone());
    assert!(
        matches!(refused, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::UnsupportedTarget)
    );
    fallback["lexicalFallback"] = json!(true);
    let fallback = searched(f.query(&b, "documents.search", fallback));
    assert!(matches!(fallback.effective_mode, RetrievalMode::Lexical));
    assert_eq!(fallback.passages.len(), 2);
}

#[test]
fn citation_resolution_is_negotiated_and_never_retargets_a_forged_locator_or_digest() {
    use devworks_core::protocol::{data::Locator, provider::Digest};
    let _case = native_runtime_case();
    let h = Harness::new();
    let (f, indexer) = compose(&h);
    let a = f.scoped("document-a");
    let note = put(&f, &indexer, &a, "cited", b"A precise block.", None);
    let exact = note.current_version.as_ref().unwrap();
    let citation = search(&f, &a, "precise", &[]).passages[0].citation.clone();
    assert_eq!(open(&f, &a, &citation).text.0, "A precise block.");
    assert!(matches!(
        f.query(
            &a,
            "documents.resolve_citation",
            cite_envelope(&citation, 10)
        ),
        DomainQueryReply::Rejected { .. }
    ));
    let legacy = f.read_ref(&a, "documents.read", exact, 1);
    assert!(matches!(
        result(legacy),
        OperationResult::DocumentsRead {
            value: DocumentResult::Legacy(_)
        }
    ));
    let mut forged = citation.clone();
    forged.locator = Locator::Text {
        first_line: 2,
        last_line: 2,
    };
    assert_unavailable(f.query(&a, "documents.resolve_citation", cite_envelope(&forged, 11)));
    let mut forged = citation;
    forged.content_digest = Digest::try_from("f".repeat(64)).unwrap();
    assert_unavailable(f.query(&a, "documents.resolve_citation", cite_envelope(&forged, 11)));
}

#[test]
fn gateway_rechecks_a_document_share_after_the_search_snapshot() {
    use devworks_core::{
        gateway::AdmittedQuery,
        ports::{QueryAnswer, QueryHandler, QueryPublicationCheck, storage::ReadSnapshot},
        protocol::{DomainFault, Id},
    };
    struct RevokeAfterQuery {
        library: DocumentLibrary,
        store: Arc<dyn ApplicationStore>,
        document: Id,
        recipient: ProjectId,
    }
    impl QueryHandler for RevokeAfterQuery {
        fn publication_check(&self, query: &AdmittedQuery) -> Option<QueryPublicationCheck> {
            self.library.publication_check(query)
        }
        fn query(
            &self,
            ctx: &InvocationContext,
            query: &AdmittedQuery,
            snapshot: &dyn ReadSnapshot,
        ) -> Result<QueryAnswer, DomainFault> {
            let answer = self.library.query(ctx, query, snapshot)?;
            assert!(
                matches!(&answer.result, OperationResult::DocumentsSearch { value }
                if value.passages.len() == 1)
            );
            let (document, recipient) = (self.document.clone(), self.recipient.clone());
            assert!(
                write(self.store.as_ref(), move |tx| tx
                    .artifacts()
                    .remove_share(&document, &recipient))
                .unwrap()
            );
            Ok(answer)
        }
    }
    let _case = native_runtime_case();
    let h = Harness::new();
    let (f, indexer) = compose(&h);
    let a = f.scoped("document-a");
    let b = f.scoped("document-b");
    let source = put(&f, &indexer, &a, "revocable", b"Revocable evidence.", None);
    assert!(matches!(applied(f.command(&f.human, "documents.share",
        json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"share-revocable",
            "command":{"kind":"Share","artifactId":source.id,"targetProjectId":"document-b","expectedScopeRevision":source.scope_revision}}))),
        OperationResult::DocumentsShare { .. }));
    let authority = HostAuthority {
        store: f.store.as_ref(),
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: f.store.as_ref(),
        ids: &*h.ids,
        authority: &authority,
    };
    let handler = RevokeAfterQuery {
        library: DocumentLibrary::new(f.artifacts.clone(), h.ids.clone(), h.clock()),
        store: f.store.clone(),
        document: source.id,
        recipient: b.project_id().unwrap().clone(),
    };
    let reply = gateway
        .query(
            &b,
            "documents.search",
            envelope(&b, "revocable", &[], 10, 1000)
                .to_string()
                .as_bytes(),
            &handler,
        )
        .unwrap();
    assert!(matches!(reply, DomainQueryReply::Rejected { fault, .. }
        if fault.code == DomainErrorCode::PermissionDenied));
}
