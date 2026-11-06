//! Recorded vectors over the actual qualified native route, owner accounting and core index.
use super::*;
use devworks_core::{
    gateway::{AdmittedQuery, Gateway},
    operations::{OperationInput, conventions as ops},
    ports::{Preparation, Prepared, QueryAnswer, QueryHandler},
    protocol::{
        CorrelationId, DomainErrorCode, DomainFault, OperationState, results::OperationResult,
    },
    services::{
        conventions::indexing::{
            IndexService,
            embedding::{
                ConventionEmbeddings, EmbeddedBatch, EmbeddingConfiguration, EmbeddingFailure,
                EmbeddingRequest,
            },
            publisher::IndexOutcome,
        },
        router::embedding_spaces::ProbePreparation,
        sessions::{Failure, Refusal},
    },
};
use devworks_engine::conventions::embedding::NativeEmbeddings;
use std::sync::atomic::AtomicBool;
pub(super) const VECTOR: &str = r#"{"model":"fixture-model","data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":3,"total_tokens":3}}"#;
pub(super) fn qualify(f: &Fx) -> (InvocationContext, Arc<NativeEmbeddings>, Arc<IndexService>) {
    let (spaces, operation) = super::embedding_qualification::probe(f);
    let executor = Arc::new(NativeEmbeddings::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.driver.clone(),
        spaces.clone(),
        f.h.clock(),
    ));
    let ProbePreparation::Ready(prepared) = spaces.prepare_probe(&operation).unwrap() else {
        panic!("actual native probe ready")
    };
    let caller = prepared.context().clone();
    executor.execute_probe(*prepared, false).unwrap();
    let complete = spaces.clone();
    let op = operation.id.clone();
    write(&*f.store, move |tx| {
        let actual = tx.operations().operation(&op)?.unwrap();
        tx.operations().transition(
            &op,
            actual.revision,
            actual.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::ObservedComplete,
            None,
        )?;
        complete.complete_probe(tx, &op)?;
        let q = tx
            .router()
            .embedding_qualification(&id("actual-candidate"))?
            .unwrap();
        tx.router()
            .select_embedding_candidate(1, Some((&id("actual-candidate"), q.revision)))?;
        Ok(())
    })
    .unwrap();
    let index = Arc::new(IndexService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    index.set_embeddings(Some(executor.clone()));
    (caller, executor, index)
}
fn source(
    f: &Fx,
    index: Arc<IndexService>,
    ctx: &InvocationContext,
    name: &str,
) -> (StoredConvention, OperationRecord) {
    let staged = f
        .artifacts
        .stage_host_bytes(
            &*f.store,
            b"# Policy\nalpha exact authored rule",
            json!({"fixture":"authored convention"}),
        )
        .unwrap()
        .unwrap();
    let (artifacts, pending, ctx, name) = (
        f.artifacts.clone(),
        staged.clone(),
        ctx.clone(),
        name.to_owned(),
    );
    let (row, op) = write(&*f.store, move |tx| {
        let version = artifacts.commit_host_bytes(
            tx,
            &pending,
            "authored convention",
            "text/markdown",
            json!({"fixture":"convention"}),
        )?;
        let row = StoredConvention {
            id: id(&name),
            scope: KnowledgeScope::Workspace,
            revision: 1,
            slug: name.clone(),
            title: format!("Guide {name}"),
            parent_id: None,
            sort_order: 0,
            lifecycle: "Active".into(),
            content_ref: Some(json!(version_ref(&version))),
            raw_content_ref: None,
            applicability: vec![],
            provenance: json!({"kind":"Authored","actorId":ctx.principal()}),
            author_id: ctx.principal().clone(),
            created_at_ms: tx.now_ms(),
            updated_at_ms: tx.now_ms(),
            legacy_approved: false,
        };
        tx.conventions().create_convention(&row)?;
        let op = index.record_changed(tx, &ctx, &row)?;
        let journal = tx
            .operations()
            .begin_attempt(&op, 1, AttemptBinding::default())?;
        let op = tx.operations().transition(
            &op,
            2,
            Some(&journal.fence()),
            OperationState::Dispatching,
            None,
        )?;
        Ok((row, op))
    })
    .unwrap();
    f.artifacts.settle_host_bytes(&*f.store, &staged, true);
    (row, op)
}
fn push(f: &Fx) {
    f.http
        .lock()
        .unwrap()
        .body
        .push_back(VECTOR.as_bytes().to_vec());
}
fn sent(f: &Fx) -> usize {
    f.http.lock().unwrap().requests.lock().unwrap().len()
}
struct Search(Arc<IndexService>);
fn input(query: &AdmittedQuery) -> &ops::SearchInput {
    let OperationInput::Conventions(ops::Input::Search(input)) = query.input() else {
        panic!("actual admitted search")
    };
    input
}
fn fault(error: Failure) -> DomainFault {
    match error {
        Failure::Refused(r) => r,
        Failure::Storage(StorageError::CursorExpired) => {
            Refusal::new(DomainErrorCode::CursorExpired, "search", None)
        }
        Failure::Storage(StorageError::Cancelled) => {
            Refusal::new(DomainErrorCode::Cancelled, "search", None)
        }
        Failure::Storage(error) => panic!("unexpected native search failure {error:?}"),
    }
    .fault(CorrelationId(id("search-fault")))
}
impl QueryHandler for Search {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        q: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        self.0
            .prepare_search(ctx, q, env, input(q))
            .map(|p| Some(Prepared::new(p)))
            .map_err(fault)
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        q: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let page = self
            .0
            .search(read, ctx, q, input(q), q.prepared().unwrap())
            .map_err(fault)?;
        Ok(QueryAnswer {
            stamp: read
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), page.scope_revision.clone()),
            result: OperationResult::ConventionsSearch {
                value: devworks_core::protocol::records::ConventionSearchPageResult::Current(page),
            },
        })
    }
}
fn search(
    f: &Fx,
    index: Arc<IndexService>,
    ctx: &InvocationContext,
    q: &str,
    text: &str,
    cursor: Option<&str>,
) -> Value {
    let mut frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":q,"schemaRevision":3,"body":{"kind":"conventions.search","scope":{"kind":"Workspace"},"page":{"limit":1},"applicability":[],"text":text,"topK":"2"}});
    if let Some(cursor) = cursor {
        frame["body"]["page"]["cursor"] = json!(cursor);
    }
    let gateway = Gateway {
        authority: &Authority,
        store: &*f.store,
        ids: f.h.ids.as_ref(),
    };
    json!(
        gateway
            .query(
                ctx,
                "conventions.search",
                frame.to_string().as_bytes(),
                &Search(index)
            )
            .unwrap()
    )
}
#[test]
fn native_qualified_index_and_hybrid_pages_reuse_exact_query_without_second_send() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, _, index) = qualify(&f);
    for name in ["a", "b"] {
        let (owner, op) = source(&f, index.clone(), &ctx, name);
        push(&f);
        assert!(matches!(
            index.run_index(&op, &Cancellation::new(), None).unwrap(),
            IndexOutcome::Completed(_)
        ));
        assert_eq!(
            read(&*f.store, |r| r
                .conventions()
                .convention_index_head(&owner.id))
            .unwrap()
            .status,
            "Ready"
        );
    }
    // A new queued replacement is status metadata, not a hole in the usable published corpus.
    let (service, caller) = (index.clone(), ctx.clone());
    write(&*f.store, move |tx| {
        let owner = tx
            .conventions()
            .convention(&id("a"), None, &[KnowledgeScope::Workspace])?
            .unwrap();
        service.record_changed(tx, &caller, &owner).map(|_| ())
    })
    .unwrap();
    assert_eq!(sent(&f), 3);
    push(&f);
    let first = search(&f, index.clone(), &ctx, "actual-search", "alpha", None);
    let page = &first["result"]["value"];
    assert_eq!(page["retrieval"]["mode"], "Hybrid", "{first}");
    assert!(page["retrieval"]["gaps"].as_array().unwrap().is_empty());
    let cursor = page["nextCursor"].as_str().unwrap();
    assert_eq!(sent(&f), 4);
    let next_invocation = devworks_core::context::HostIngress::new(f.store.as_ref()).context(
        ctx.principal().clone(),
        ctx.principal_kind(),
        id("next-native-invocation"),
        ctx.ingress_namespace().clone(),
        Counter::new(f.store.info().restore_epoch).unwrap(),
        ctx.policy_revision().clone(),
    );
    let second = search(
        &f,
        index,
        &next_invocation,
        "actual-search-page-two",
        "alpha",
        Some(cursor),
    );
    assert_eq!(
        second["result"]["value"]["retrieval"]["mode"], "Hybrid",
        "{second}"
    );
    assert_eq!(
        sent(&f),
        4,
        "continuation observes actual durable native result only"
    );
    assert!(f.transcript().is_empty());
}
struct LoseAck {
    native: Arc<NativeEmbeddings>,
    first: AtomicBool,
}
impl ConventionEmbeddings for LoseAck {
    fn is_available(&self) -> bool {
        self.native.is_available()
    }
    fn configured_space(
        &self,
        c: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, EmbeddingFailure> {
        self.native.configured_space(c)
    }
    fn embed(
        &self,
        c: &InvocationContext,
        r: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        let request = r.request_id.clone();
        let result = self.native.embed(c, r)?;
        if self.first.swap(false, Ordering::SeqCst) {
            Err(EmbeddingFailure::Unknown {
                request_id: request,
                reason: "controlled lost native acknowledgement".into(),
            })
        } else {
            Ok(result)
        }
    }
    fn observe(
        &self,
        c: &InvocationContext,
        r: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        self.native.observe(c, r)
    }
}
#[test]
fn lost_native_ack_is_observed_then_published_without_duplicate_http_or_attempt() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, index) = qualify(&f);
    index.set_embeddings(Some(Arc::new(LoseAck {
        native,
        first: AtomicBool::new(true),
    })));
    let (owner, operation) = source(&f, index.clone(), &ctx, "lost-ack");
    push(&f);
    assert!(matches!(
        index
            .run_index(&operation, &Cancellation::new(), None)
            .unwrap(),
        IndexOutcome::Unknown(_)
    ));
    assert_eq!(sent(&f), 2);
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
    let op = operation.id.clone();
    let operation = write(&*f.store, move |tx| {
        let current = tx.operations().operation(&op)?.unwrap();
        tx.operations().transition(
            &op,
            current.revision,
            current.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::OutcomeUnknown,
            None,
        )
    })
    .unwrap();
    assert!(matches!(
        index
            .run_index(&operation, &Cancellation::new(), None)
            .unwrap(),
        IndexOutcome::Completed(_)
    ));
    assert_eq!(sent(&f), 2);
    assert_eq!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .unwrap()
        .status,
        "Ready"
    );
}
#[test]
fn blank_or_empty_corpus_never_starts_paid_query_and_selection_change_fences_index() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, _, index) = qualify(&f);
    let empty = search(&f, index.clone(), &ctx, "empty-corpus", "alpha", None);
    assert!(
        empty["result"]["value"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(sent(&f), 1);
    let (_, ready) = source(&f, index.clone(), &ctx, "ready");
    push(&f);
    index.run_index(&ready, &Cancellation::new(), None).unwrap();
    let blank = search(&f, index.clone(), &ctx, "blank-query", " \t\u{2003}", None);
    assert!(
        blank["result"]["value"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sent(&f),
        2,
        "even an available qualified route and ready corpus must not embed whitespace"
    );
    let (_, op) = source(&f, index.clone(), &ctx, "queued");
    write(&*f.store, |tx| {
        tx.router().select_embedding_candidate(2, None).map(|_| ())
    })
    .unwrap();
    assert!(index.run_index(&op, &Cancellation::new(), None).is_err());
    assert_eq!(sent(&f), 2);
}

#[test]
fn query_registers_actual_new_selected_space_over_existing_lexical_index() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let author = principal(
        &*f.store,
        "author-before-qualification",
        PrincipalKind::Human,
    );
    let lexical = Arc::new(IndexService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    let (_, op) = source(&f, lexical.clone(), &author, "old-lexical");
    lexical.run_index(&op, &Cancellation::new(), None).unwrap();
    assert_eq!(sent(&f), 0);
    let (ctx, _, index) = qualify(&f);
    assert_eq!(sent(&f), 1);
    push(&f);
    let reply = search(&f, index, &ctx, "new-space-query", "alpha", None);
    let page = &reply["result"]["value"];
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "{reply}");
    assert_eq!(page["retrieval"]["mode"], "Hybrid");
    assert_eq!(sent(&f), 2);
    assert!(
        page["retrieval"]["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g.as_str().unwrap().contains("MissingCompatibleVectors"))
    );
    assert!(f.transcript().is_empty());
}
#[test]
fn actual_unknown_query_keeps_original_financial_custody_after_index_witness_changes() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, index) = qualify(&f);
    let (_, op) = source(&f, index.clone(), &ctx, "source");
    push(&f);
    index.run_index(&op, &Cancellation::new(), None).unwrap();
    let (index, first) = interrupted_query(&f, &ctx, native, index);
    let page = &first["result"]["value"];
    assert_eq!(page["retrieval"]["mode"], "Lexical", "{first}");
    let request: Id =
        serde_json::from_value(page["retrieval"]["embeddingRequestId"].clone()).unwrap();
    let operation: devworks_core::protocol::OperationId =
        serde_json::from_value(page["retrieval"]["embeddingOperation"]["operationId"].clone())
            .unwrap();
    let original = read(&*f.store, |r| r.operations().operation(&operation)).unwrap();
    assert_eq!(original.payload["requestId"], json!(request));
    assert!(matches!(
        original.state,
        OperationState::OutcomeUnknown | OperationState::Dispatching
    ));
    assert_eq!(sent(&f), 3);
    let (service, caller) = (index.clone(), ctx.clone());
    write(&*f.store, move |tx| {
        let owner = tx
            .conventions()
            .convention(&id("source"), None, &[KnowledgeScope::Workspace])?
            .unwrap();
        service.record_changed(tx, &caller, &owner).map(|_| ())
    })
    .unwrap();
    let repeated = search(&f, index, &ctx, "same-financial-query", "alpha", None);
    assert_eq!(repeated["fault"]["code"], "CursorExpired", "{repeated}");
    assert_eq!(
        sent(&f),
        3,
        "a changed corpus witness must not mint new query custody or send a second request"
    );
    let after = read(&*f.store, |r| {
        r.operations().operations_for_capability(
            "conventions.embedding_prepare",
            &[
                OperationState::Dispatching,
                OperationState::OutcomeUnknown,
                OperationState::ObservedComplete,
                OperationState::Failed,
            ],
            None,
            500,
        )
    });
    let queries = after
        .iter()
        .filter(|op| op.payload["origin"]["kind"] == "Query")
        .collect::<Vec<_>>();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].id, operation);
    assert_eq!(queries[0].args_digest, original.args_digest);
}

fn interrupted_query(
    f: &Fx,
    ctx: &InvocationContext,
    native: Arc<NativeEmbeddings>,
    index: Arc<IndexService>,
) -> (Arc<IndexService>, Value) {
    let waiting = {
        let mut script = f.http.lock().unwrap();
        script.wait_for_cancel = true;
        script.waiting.clone()
    };
    std::thread::scope(|scope| {
        let worker =
            scope.spawn(|| search(f, index.clone(), ctx, "same-financial-query", "alpha", None));
        runtime().block_on(common::within(
            "actual query transport starts",
            waiting.notified(),
        ));
        index.stop();
        let cancelled = worker.join().unwrap();
        assert_eq!(cancelled["fault"]["code"], "Cancelled");
    });
    f.http.lock().unwrap().wait_for_cancel = false;
    let query = read(&*f.store, |r| {
        r.operations().operations_for_capability(
            "conventions.embedding_prepare",
            &[
                OperationState::Dispatching,
                OperationState::OutcomeUnknown,
                OperationState::Failed,
            ],
            None,
            500,
        )
    })
    .into_iter()
    .find(|op| op.payload["origin"]["kind"] == "Query")
    .unwrap();
    let request: Id = serde_json::from_value(query.payload["requestId"].clone()).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let ready = read(&*f.store, |r| {
            let rows = r
                .usage()
                .inference_attempts(&UsageAttemptFilter::default(), None, 500)?;
            let attempt = rows.iter().find(|a| a.request_id == request);
            attempt
                .map(|a| r.usage().attempt_balance(&a.attempt_id))
                .transpose()
                .map(|v| {
                    v.flatten()
                        .is_some_and(|b| b.local_quiescent && b.unknown_tail)
                })
        });
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "actual interrupted native attempt must settle its unknown tail"
        );
        std::thread::yield_now();
    }
    let index = Arc::new(IndexService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    index.set_embeddings(Some(native));
    let first = search(f, index.clone(), ctx, "same-financial-query", "alpha", None);
    (index, first)
}

#[test]
fn original_unknown_custody_cannot_disappear_when_selection_clears_or_corpus_becomes_empty() {
    for clear_space in [true, false] {
        let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
        let (ctx, native, index) = qualify(&f);
        let (owner, op) = source(&f, index.clone(), &ctx, "source");
        push(&f);
        index.run_index(&op, &Cancellation::new(), None).unwrap();
        let (index, first) = interrupted_query(&f, &ctx, native, index);
        let original = first["result"]["value"]["retrieval"]["embeddingRequestId"].clone();
        assert!(original.is_string());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 3);
        if clear_space {
            super::selection_refresh::clear(&f, index.clone(), &ctx);
        } else {
            let mut retired = owner;
            retired.revision += 1;
            retired.lifecycle = "Retired".into();
            write(&*f.store, move |tx| {
                tx.conventions().revise_convention(&retired, 1)
            })
            .unwrap();
        }
        let repeated = search(
            &f,
            index.clone(),
            &ctx,
            "same-financial-query",
            "alpha",
            None,
        );
        assert_eq!(
            repeated["fault"]["code"], "CursorExpired",
            "clearSpace={clear_space}: {repeated}"
        );
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 3);
        let deliberate = search(&f, index, &ctx, "deliberate-new-query", "alpha", None);
        assert_eq!(
            deliberate["result"]["value"]["retrieval"]["mode"], "Lexical",
            "{deliberate}"
        );
        assert!(deliberate["result"]["value"]["retrieval"]["embeddingRequestId"].is_null());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 3);
        let rows = read(&*f.store, |r| {
            r.operations().operations_for_capability(
                "conventions.embedding_prepare",
                &[
                    OperationState::Dispatching,
                    OperationState::OutcomeUnknown,
                    OperationState::ObservedComplete,
                    OperationState::Failed,
                ],
                None,
                500,
            )
        });
        let queries = rows
            .iter()
            .filter(|op| op.payload["origin"]["kind"] == "Query")
            .collect::<Vec<_>>();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].payload["requestId"], original);
    }
}
