use super::{
    indexing::{Processor, drive, imported_result, index_frame, indexed, management_frame, run_id},
    *,
};
use devworks_core::{
    protocol::{
        Id,
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

fn coordinator(
    f: &Fixture<'_>,
    control: &Arc<devworks_core::services::runs::ControlService>,
    root: &str,
) -> InvocationContext {
    use devworks_core::{
        context::HostIngress,
        protocol::{Counter, Revision},
        services::agents::AgentService,
    };
    let started = f.command(
        &f.human,
        "runs.start",
        json!({"protocol":"1.0-draft.1","requestKey":format!("start-{root}"),
            "command":{"kind":"runs.start","rootId":root,
                "expectedWorkRevision":"1","expectedControlRevision":"1"}}),
    );
    assert!(
        matches!(&started, CommandReply::Recorded { receipt, .. }
            if !matches!(receipt.decision, DomainDecision::Refused { .. })),
        "{started:?}"
    );
    let control = control.clone();
    let ids = f.h.ids.clone();
    let host = principal(f.store.as_ref(), "transfer-host", PrincipalKind::Host);
    let (assignment, session, activation) = write(f.store.as_ref(), move |tx| {
        let lease = control
            .claim_activation(tx, &id("transfer-owner"))?
            .unwrap();
        let activation = control
            .start_activation(tx, &lease.id, lease.owner_epoch)?
            .unwrap();
        let service = AgentService::new(ids, control);
        let primary = service
            .materialize_primary(tx, &host, &activation.id, activation.owner_epoch)
            .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
        let assignment = tx.runs().assignment(&primary.assignment_id)?.unwrap();
        let session = tx.sessions().session(&assignment.session_id)?.unwrap();
        Ok((assignment, session, activation))
    })
    .unwrap();
    HostIngress::new(f.store.as_ref())
        .context(
            session.agent_id,
            PrincipalKind::Agent,
            id(&format!("{root}-invocation")),
            id(&format!("{root}-ingress")),
            Counter::new(f.store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(session.id)
        .with_lineage(session.project_id, session.root_id, Some(assignment.id))
        .with_activation(activation.id, activation.owner_epoch)
}

fn transfer_proposal(
    f: &Fixture<'_>,
    document: &ArtifactRef,
    destination: &str,
    source_needs: bool,
    key: &str,
) -> Value {
    let current = read(f.store.as_ref(), |r| {
        r.artifacts().artifact(&document.artifact_id)
    })
    .unwrap();
    json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":14,"requestKey":key,
        "body":{"kind":"documents.transfer_propose","documentId":document.artifact_id,
            "expectedDocumentRevision":current.revision.to_string(),
            "expectedScopeRevision":current.scope_revision.to_string(),"destinationRootId":destination,
            "sourceNeedsDocument":source_needs,"summary":"Relevant coordinator notes"}})
}

fn transfer_response(transfer: &Id, need: bool, key: &str) -> Value {
    json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":14,"requestKey":key,
        "body":{"kind":"documents.transfer_respond","transferId":transfer,
            "expectedRevision":"1","destinationNeedsDocument":need}})
}

fn transfer_status(f: &Fixture<'_>, ctx: &InvocationContext, transfer: &Id) -> DomainQueryReply {
    f.query(
        ctx,
        "documents.transfer_status",
        json!({
            "protocol":"1.0-draft.1","mode":"Query","schemaRevision":14,"queryId":"transfer-status",
            "body":{"kind":"documents.transfer_status","transferId":transfer}
        }),
    )
}

fn proposal_id(reply: CommandReply<DomainDecision>) -> Id {
    let OperationResult::DocumentsTransferPropose { value } = applied(reply) else {
        panic!("expected document proposal");
    };
    value.id
}

fn confirm_decision(reply: CommandReply<DomainDecision>, state: &str) {
    let OperationResult::DocumentsTransferRespond { value } = applied(reply) else {
        panic!("expected document transfer decision");
    };
    assert_eq!(serde_json::to_value(value.state).unwrap(), state);
}

#[test]
fn native_document_transfer_requires_both_coordinators_and_preserves_exact_evidence() {
    use devworks_core::{
        authority::service::revoke_principal,
        services::{agents::AgentService, runs::ControlService},
    };
    let _case = native_runtime_case();
    let h = Harness::new();
    let (completed, receiving_assignment) = {
        let (mut f, indexer) = compose(&h);
        seed::rows(
            f.store.as_ref(),
            &[seed::ProjectSeed::new("document-c")],
            &[
                seed::WorkSeed::issue("document-source-root", "document-a"),
                seed::WorkSeed::issue("document-destination-root", "document-b"),
            ],
        )
        .unwrap();
        let control = Arc::new(ControlService::new(h.ids.clone()));
        f.cases = f
            .cases
            .with_runs(control.clone())
            .with_agents(Arc::new(AgentService::new(h.ids.clone(), control.clone())));
        let a = coordinator(&f, &control, "document-source-root");
        let b = coordinator(&f, &control, "document-destination-root");
        let c = f.scoped("document-c");
        let a_view = f.scoped("document-a");
        let b_view = f.scoped("document-b");
        let doc = put(
            &f,
            &indexer,
            &a_view,
            "transfer-doc",
            b"Greenhouse evidence.",
            None,
        );
        let reference = doc.current_version.as_ref().unwrap();
        let citation = search(&f, &a_view, "greenhouse", &[]).passages[0]
            .citation
            .clone();
        let scope = &doc.scope_revision;
        applied(f.command(
            &f.human,
            "documents.share",
            json!({"protocol":"1.0-draft.1","schemaRevision":10,
            "requestKey":"initial-share-to-c","command":{"kind":"Share","artifactId":doc.id,
                "targetProjectId":"document-c","expectedScopeRevision":scope}}),
        ));
        let proposed = transfer_proposal(&f, reference, "document-destination-root", true, "share");
        let share = proposal_id(f.command(&a, "documents.transfer_propose", proposed.clone()));
        let replay = f.command(&a, "documents.transfer_propose", proposed.clone());
        assert!(matches!(
            replay,
            CommandReply::Recorded { replayed: true, .. }
        ));
        assert_eq!(proposal_id(replay), share);
        let mut changed = proposed.clone();
        changed["body"]["sourceNeedsDocument"] = json!(false);
        assert!(matches!(
            f.command(&a, "documents.transfer_propose", changed),
            CommandReply::Rejected { .. }
        ));
        let project = b.project_id().unwrap().clone();
        let assignment = b.assignment_id().unwrap().clone();
        let notifications = read(f.store.as_ref(), move |r| {
            r.runs()
                .agent_assignment_thread(&project, &assignment, "Inbound", true, None, 20)
        });
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].0.body.contains(share.as_str()));
        assert_eq!(notifications[0].0.attachments, json!([]));
        assert!(search(&f, &b_view, "greenhouse", &[]).passages.is_empty());
        let forged = f.command(
            &a,
            "documents.transfer_respond",
            transfer_response(&share, true, "own-answer"),
        );
        assert!(matches!(
            forged, CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        let response = transfer_response(&share, true, "share-answer");
        confirm_decision(
            f.command(&b, "documents.transfer_respond", response.clone()),
            "Shared",
        );
        assert!(matches!(
            f.command(&b, "documents.transfer_respond", response),
            CommandReply::Recorded { replayed: true, .. }
        ));
        assert_eq!(open(&f, &a_view, &citation).text.0, "Greenhouse evidence.");
        assert_eq!(open(&f, &b_view, &citation).text.0, "Greenhouse evidence.");

        let declined = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(&f, reference, "document-destination-root", false, "decline"),
        ));
        confirm_decision(
            f.command(
                &b,
                "documents.transfer_respond",
                transfer_response(&declined, false, "decline-answer"),
            ),
            "Declined",
        );
        let stale = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(&f, reference, "document-destination-root", false, "stale"),
        ));
        let actor = a.principal().clone();
        let human = f.human.clone();
        write(f.store.as_ref(), move |tx| {
            let existing = tx.authority().principal(&actor)?.unwrap();
            let revoked = revoke_principal(tx, &human, &actor, existing.revision)
                .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
            bind_principal(
                tx,
                &human,
                NewPrincipalBinding {
                    actor,
                    kind: existing.kind,
                    role: existing.role,
                    project_id: existing.project_id,
                    tool_families: existing.tool_families,
                    bound_by: human.principal().clone(),
                },
                Some(revoked.revision),
            )
            .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
            Ok(())
        })
        .unwrap();
        let revoked_status = transfer_status(&f, &a, &stale);
        assert!(matches!(
            revoked_status, DomainQueryReply::Rejected { fault, .. }
                if fault.code == DomainErrorCode::PermissionDenied
        ));
        let old = f.command(
            &b,
            "documents.transfer_respond",
            transfer_response(&stale, true, "stale-answer"),
        );
        assert!(matches!(
            old, CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        let before_setting = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(
                &f,
                reference,
                "document-destination-root",
                false,
                "before-setting",
            ),
        ));
        let setting = devworks_core::settings::catalog::descriptor("tools.overrides")
            .unwrap()
            .schema_ref();
        let select_proposal = |previous, selected| {
            let scope = devworks_core::ports::storage::ConfigScope::Workspace;
            let setting = setting.clone();
            write(f.store.as_ref(), move |tx| {
                tx.configuration().append_revision(
                    &scope,
                    "settings",
                    previous,
                    1,
                    json!({"values":{"tools.overrides":{"schema":setting,
                      "value":{"devworks/documents.transfer_propose":selected}}}}),
                )?;
                Ok(())
            })
            .unwrap();
        };
        select_proposal(1, false);
        assert!(matches!(
            f.command(&b, "documents.transfer_respond",
                transfer_response(&before_setting, true, "disabled-source")),
            CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        select_proposal(2, true);
        let before_archive = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(
                &f,
                reference,
                "document-destination-root",
                false,
                "before-archive",
            ),
        ));
        let project = a.project_id().unwrap().clone();
        write(f.store.as_ref(), move |tx| {
            // The lifecycle service advances this epoch on archive, then restores Active.
            let mut stored = tx.projects().project(&project)?.unwrap();
            let prior = stored.revision;
            stored.revision += 1;
            stored.archive_epoch += 1;
            tx.projects().update_project(&stored, prior)
        })
        .unwrap();
        assert!(matches!(
            f.command(&b, "documents.transfer_respond",
                transfer_response(&before_archive, true, "after-reopen")),
            CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        let contrary = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(
                &f,
                reference,
                "document-destination-root",
                false,
                "contrary",
            ),
        ));
        let retain = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(&f, reference, "document-destination-root", true, "retain"),
        ));
        confirm_decision(
            f.command(
                &b,
                "documents.transfer_respond",
                transfer_response(&retain, true, "retain-answer"),
            ),
            "Shared",
        );
        assert!(matches!(
            f.command(&b, "documents.transfer_respond",
                transfer_response(&contrary, true, "contrary-answer")),
            CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        let competing = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(
                &f,
                reference,
                "document-destination-root",
                false,
                "competing",
            ),
        ));
        let moved = proposal_id(f.command(
            &a,
            "documents.transfer_propose",
            transfer_proposal(&f, reference, "document-destination-root", false, "move"),
        ));
        confirm_decision(
            f.command(
                &b,
                "documents.transfer_respond",
                transfer_response(&moved, true, "move-answer"),
            ),
            "Moved",
        );
        let stale = f.command(
            &b,
            "documents.transfer_respond",
            transfer_response(&competing, true, "competing-answer"),
        );
        assert!(matches!(
            stale, CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        assert_unavailable(f.query(
            &a_view,
            "documents.resolve_citation",
            cite_envelope(&citation, 11),
        ));
        assert!(search(&f, &a_view, "greenhouse", &[]).passages.is_empty());
        assert_eq!(open(&f, &b_view, &citation).text.0, "Greenhouse evidence.");
        assert_eq!(open(&f, &c, &citation).text.0, "Greenhouse evidence.");
        let retained_assignment = a.assignment_id().unwrap().clone();
        write(f.store.as_ref(), move |tx| {
            let mut newer = tx.runs().assignment(&retained_assignment)?.unwrap();
            let prior = newer.revision;
            newer.revision += 1;
            newer.generation += 1;
            tx.runs().update_assignment(&newer, prior)
        })
        .unwrap();
        let status = result(transfer_status(&f, &a, &moved));
        assert!(matches!(
            status,
            OperationResult::DocumentsTransferStatus { .. }
        ));
        let sharing_back = proposal_id(f.command(
            &b,
            "documents.transfer_propose",
            transfer_proposal(&f, reference, "document-source-root", true, "share-back"),
        ));
        confirm_decision(
            f.command(
                &a,
                "documents.transfer_respond",
                transfer_response(&sharing_back, true, "share-back-answer"),
            ),
            "Shared",
        );
        assert_eq!(open(&f, &a_view, &citation).text.0, "Greenhouse evidence.");
        let source_project = ProjectId(id("document-a"));
        let plan = read(f.store.as_ref(), |r| {
            r.projects().purge_planning(&source_project)
        })
        .unwrap();
        assert!(plan.rows.iter().any(|row| {
            row.table == "document_transfers"
                && row.action == devworks_core::ports::storage::ProjectPurgeRowAction::Delete
        }));
        (moved, b.assignment_id().unwrap().clone())
    };
    let f = Fixture::open(&h);
    let recovered = read(f.store.as_ref(), |r| {
        r.documents().document_transfer(&completed)
    })
    .unwrap();
    assert_eq!(serde_json::to_value(&recovered.state).unwrap(), "Moved");
    assert_eq!(recovered.revision.get(), 2);
    let deliveries = read(f.store.as_ref(), |r| {
        r.runs().agent_assignment_thread(
            &ProjectId(id("document-b")),
            &receiving_assignment,
            "Inbound",
            true,
            None,
            20,
        )
    });
    assert!(
        deliveries
            .iter()
            .any(|(message, _)| message.body.contains(completed.as_str()))
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
        matches!(refused, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::ViewUnavailable)
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
    let mut omitted = cite_envelope(&citation, 11);
    omitted.as_object_mut().unwrap().remove("schemaRevision");
    assert!(matches!(
        f.query(&a, "documents.resolve_citation", omitted),
        DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::SchemaMismatch
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
fn gateway_rechecks_a_document_share_after_search_and_activity_snapshots() {
    use devworks_core::{
        gateway::AdmittedQuery,
        ports::{QueryAnswer, QueryHandler, QueryPublicationCheck, storage::ReadSnapshot},
        protocol::{
            DomainFault, Id,
            data::{DataScope, DocumentVersions},
        },
        services::documents::evidence::DocumentSearchWitness,
    };
    struct RevokeAfterQuery {
        library: DocumentLibrary,
        store: Arc<dyn ApplicationStore>,
        document: Id,
        recipient: ProjectId,
        expected_text: Option<&'static str>,
        expected_completion: QueryCompletion,
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
            match &answer.result {
                OperationResult::DocumentsSearch { value } => {
                    assert_eq!(
                        value.passages.len(),
                        usize::from(self.expected_text.is_some())
                    );
                    assert_eq!(
                        value.passages.first().map(|passage| passage.text.as_str()),
                        self.expected_text
                    );
                    assert_eq!(value.completion, self.expected_completion);
                }
                OperationResult::DocumentsRead {
                    value: DocumentResult::Current(value),
                } => {
                    assert!(
                        value
                            .index_activity
                            .as_ref()
                            .is_some_and(|activity| activity.chunk_count.is_some())
                    );
                }
                unexpected => panic!("{unexpected:?}"),
            }
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
    let recipient = b.project_id().unwrap().clone();
    for (query, limit, visited, expected_text, completion) in [
        (
            "revocable",
            10,
            1_000,
            Some("Revocable evidence."),
            QueryCompletion::Complete,
        ),
        ("revocable absent", 10, 1, None, QueryCompletion::WorkLimit),
        (
            "revocable retained",
            1,
            1_000,
            Some("Revocable retained evidence."),
            QueryCompletion::ResultLimit,
        ),
    ] {
        if limit == 1 {
            put(
                &f,
                &indexer,
                &b,
                "retained",
                expected_text.unwrap().as_bytes(),
                None,
            );
        }
        let (document, project) = (source.id.clone(), recipient.clone());
        assert!(
            write(f.store.as_ref(), move |tx| tx
                .artifacts()
                .add_share(&document, &project))
            .unwrap()
        );
        let witness = read(f.store.as_ref(), |snapshot| {
            DocumentSearchWitness::capture(
                snapshot,
                &b,
                &DataScope::Project {
                    project_id: recipient.clone(),
                },
                &DocumentVersions::Current,
            )
        })
        .unwrap();
        let ctx = b.clone();
        let witness = Arc::new(witness);
        let before = witness.clone();
        assert!(write(f.store.as_ref(), move |tx| before.holds(tx, &ctx)).unwrap());
        let handler = RevokeAfterQuery {
            library: DocumentLibrary::new(f.artifacts.clone(), h.ids.clone(), h.clock()),
            store: f.store.clone(),
            document: source.id.clone(),
            recipient: recipient.clone(),
            expected_text,
            expected_completion: completion,
        };
        let reply = gateway
            .query(
                &b,
                "documents.search",
                envelope(&b, query, &[], limit, visited)
                    .to_string()
                    .as_bytes(),
                &handler,
            )
            .unwrap();
        assert!(
            matches!(&reply, DomainQueryReply::Rejected { fault, .. }
            if fault.code == DomainErrorCode::PermissionDenied),
            "{reply:?}"
        );
        let ctx = b.clone();
        assert!(!write(f.store.as_ref(), move |tx| witness.holds(tx, &ctx)).unwrap());
    }
    let (document, project) = (source.id.clone(), recipient.clone());
    assert!(
        write(f.store.as_ref(), move |tx| tx
            .artifacts()
            .add_share(&document, &project))
        .unwrap()
    );
    let exact = source.current_version.as_ref().unwrap();
    let handler = RevokeAfterQuery {
        library: DocumentLibrary::new(f.artifacts.clone(), h.ids.clone(), h.clock()),
        store: f.store.clone(),
        document: source.id.clone(),
        recipient,
        expected_text: None,
        expected_completion: QueryCompletion::Complete,
    };
    let read = json!({"protocol":"1.0-draft.1","schemaRevision":13,"queryId":"document-activity-revoked",
        "limit":20,"query":{"kind":"Read","artifactId":exact.artifact_id,"versionId":exact.version_id}});
    let reply = gateway
        .query(&b, "documents.read", read.to_string().as_bytes(), &handler)
        .unwrap();
    assert_unavailable(reply);
}

#[test]
fn a_citation_tombstoned_after_the_read_returns_typed_unavailability() {
    use devworks_core::{
        gateway::AdmittedQuery,
        ports::{QueryAnswer, QueryHandler, QueryPublicationCheck, storage::ReadSnapshot},
        protocol::{DomainFault, Id},
    };

    struct TombstoneAfterQuery {
        library: DocumentLibrary,
        store: Arc<dyn ApplicationStore>,
        version: Id,
    }
    impl QueryHandler for TombstoneAfterQuery {
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
            assert!(matches!(
                &answer.result,
                OperationResult::DocumentsResolveCitation { .. }
            ));
            let version = self.version.clone();
            write(self.store.as_ref(), move |tx| {
                tx.artifacts().tombstone_version(&version, 20)
            })
            .unwrap();
            Ok(answer)
        }
    }
    let _case = native_runtime_case();
    let h = Harness::new();
    let (f, indexer) = compose(&h);
    let project = f.scoped("document-a");
    let source = put(
        &f,
        &indexer,
        &project,
        "tombstoned",
        b"A retained citation.",
        None,
    );
    let citation = search(&f, &project, "citation", &[]).passages[0]
        .citation
        .clone();
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
    let handler = TombstoneAfterQuery {
        library: DocumentLibrary::new(f.artifacts.clone(), h.ids.clone(), h.clock()),
        store: f.store.clone(),
        version: source.current_version.unwrap().version_id,
    };
    let reply = gateway
        .query(
            &project,
            "documents.resolve_citation",
            cite_envelope(&citation, 11).to_string().as_bytes(),
            &handler,
        )
        .unwrap();
    assert_unavailable(reply);
}
