//! Knowledge claim vectors over the real qualified router, producer journal, SQLite vector
//! store and Knowledge gateway. Transport replies are recorded vectors, with no learned model.
use super::{
    convention_search::{VECTOR, qualify},
    document_embeddings::{command, sent},
    *,
};
use devworks_core::{
    authority::HostAuthority,
    gateway::Gateway,
    protocol::{
        ProjectId,
        records::{Diagnostic, KnowledgeClaim},
        results::{DomainQueryReply, OperationResult},
        support::Page,
    },
    services::{
        UseCases,
        conventions::indexing::IndexService,
        knowledge::{KnowledgeService, VECTOR_CONSUMER},
    },
};
use devworks_engine::knowledge::claim_vector_consumer;
use devworks_test_support::seed;

/// One recorded embedding reply: a vector per input, in input order.
fn reply(f: &Fx, vectors: &[[f32; 2]]) {
    let data: Vec<Value> = vectors
        .iter()
        .enumerate()
        .map(|(index, v)| json!({"index": index, "embedding": v}))
        .collect();
    let body = json!({"model": "fixture-model", "data": data,
        "usage": {"prompt_tokens": 3, "total_tokens": 3}});
    f.http
        .lock()
        .unwrap()
        .body
        .push_back(body.to_string().into_bytes());
}

fn project(ctx: &InvocationContext, name: &str) -> InvocationContext {
    ctx.clone()
        .with_lineage(Some(ProjectId(id(name))), None, None)
}

fn scope(ctx: &InvocationContext) -> Value {
    json!({"kind": "Project", "projectId": ctx.project_id()})
}

/// Records one claim about `subject` saying `value`, returning its id.
fn record(f: &Fx, cases: &UseCases, ctx: &InvocationContext, key: &str, value: &str) -> Id {
    let result = command(
        f,
        cases,
        ctx,
        "knowledge.record",
        json!({"protocol": "1.0-draft.1", "mode": "Command", "requestKey": key,
            "schemaRevision": 2, "body": {"kind": "knowledge.record", "scope": scope(ctx),
            "batch": {"entities": [{"clientId": "s", "label": "Garage", "entityType": "topic",
                "aliases": []}], "evidence": [],
                "claims": [{"kind": "Fact", "subjectId": "s", "predicate": "describes",
                    "value": value, "confidence": 0.7, "evidenceIds": [], "tags": []}]}}}),
    );
    let OperationResult::KnowledgeRecord { value } = result else {
        panic!("{result:?}")
    };
    value.claim_ids.0[0].clone()
}

fn query(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    op: &str,
    body: Value,
) -> OperationResult {
    let authority = HostAuthority {
        store: f.store.as_ref(),
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: f.store.as_ref(),
        ids: &*f.h.ids,
        authority: &authority,
    };
    let mut body = body;
    body["kind"] = json!(op);
    let key = format!("q-{}", f.h.ids.next_id(IdKind::Correlation).as_str());
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": key,
        "schemaRevision": 2, "body": body});
    match cases
        .query(&gateway, ctx, op, frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("{other:?}"),
    }
}

fn search(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    text: &str,
    limit: u32,
    cursor: Option<&str>,
) -> Page<KnowledgeClaim> {
    let mut page = json!({"limit": limit});
    if let Some(cursor) = cursor {
        page["cursor"] = json!(cursor);
    }
    let body = json!({"scope": scope(ctx), "text": text, "tags": [], "kinds": [],
        "page": page, "entityLimit": 50, "claimLimit": 50});
    match query(f, cases, ctx, "knowledge.search", body) {
        OperationResult::KnowledgeSearch { value } => value,
        other => panic!("{other:?}"),
    }
}

fn ids(page: &Page<KnowledgeClaim>) -> Vec<Id> {
    page.items.0.iter().map(|c| c.id.clone()).collect()
}

fn retrieval(f: &Fx, cases: &UseCases, ctx: &InvocationContext) -> Diagnostic {
    match query(
        f,
        cases,
        ctx,
        "knowledge.diagnostics",
        json!({"scope": scope(ctx)}),
    ) {
        OperationResult::KnowledgeDiagnostics { value } => value
            .items
            .0
            .into_iter()
            .find(|d| d.component.0 == "knowledge.retrieval")
            .unwrap(),
        other => panic!("{other:?}"),
    }
}

fn knowledge(f: &Fx, index: Option<Arc<IndexService>>) -> (Arc<KnowledgeService>, UseCases) {
    let service = Arc::new(KnowledgeService::new(f.h.ids.clone(), f.h.clock(), None));
    service.set_embeddings(index);
    let cases = UseCases::new().with_knowledge(service.clone());
    (service, cases)
}

fn embed(f: &Fx, service: &Arc<KnowledgeService>) {
    let dispatcher = dispatcher(
        &f.h,
        f.store.clone(),
        vec![claim_vector_consumer(service.clone())],
    );
    runtime()
        .block_on(within("claim vectors", dispatcher.run_until_idle()))
        .unwrap();
}

fn vector_jobs(f: &Fx) -> QueueStatus {
    read(f.store.as_ref(), |r| r.queue().status(VECTOR_CONSUMER))
}

#[test]
fn a_claim_that_shares_no_word_is_found_by_meaning_and_only_in_its_own_project() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[
            seed::ProjectSeed::new("knowledge-a"),
            seed::ProjectSeed::new("knowledge-b"),
        ],
        &[],
    )
    .unwrap();
    let (a, b) = (
        project(&human, "knowledge-a"),
        project(&human, "knowledge-b"),
    );
    let (service, cases) = knowledge(&f, Some(index.clone()));
    let probes = sent(&f);

    // Each write's delivery embeds its scope's one pending claim, through the router.
    let near = record(&f, &cases, &a, "near", "a vehicle parked outside");
    reply(&f, &[[0.9, 0.1]]);
    embed(&f, &service);
    let far = record(&f, &cases, &a, "far", "kitchen recipe");
    reply(&f, &[[0.0, 1.0]]);
    embed(&f, &service);
    // Another project's claim is the nearest of all to the query.
    let other = record(&f, &cases, &b, "other", "a vehicle parked outside");
    reply(&f, &[[1.0, 0.0]]);
    embed(&f, &service);
    assert_eq!(
        sent(&f) - probes,
        3,
        "one embedding request per pending batch"
    );

    // No query word is in the claim: it is found by its vector, and the other project's
    // nearer claim is never a match of this project's search.
    reply(&f, &[[1.0, 0.0]]);
    let found = search(&f, &cases, &a, "automobile", 50, None);
    assert_eq!(ids(&found), std::slice::from_ref(&near));
    let diagnostic = retrieval(&f, &cases, &a);
    assert_eq!(diagnostic.code.0, "Hybrid");
    let space = read(f.store.as_ref(), |r| index.configuration_read(r, &a))
        .space
        .unwrap()
        .id;
    assert!(
        diagnostic
            .message
            .0
            .contains(&format!("embedding space {space} ")),
        "{}",
        diagnostic.message.0
    );
    assert!(
        diagnostic.message.0.contains("2 of 2 claims"),
        "{}",
        diagnostic.message.0
    );

    // Text matches come first, then vector matches; the cursor resumes across the two.
    reply(&f, &[[1.0, 0.0]]);
    let first = search(&f, &cases, &a, "kitchen", 1, None);
    assert_eq!(ids(&first), std::slice::from_ref(&far));
    reply(&f, &[[1.0, 0.0]]);
    let second = search(
        &f,
        &cases,
        &a,
        "kitchen",
        1,
        first.next_cursor.as_ref().map(|c| c.0.as_str()),
    );
    assert_eq!(ids(&second), [near]);
    assert!(second.next_cursor.is_none());

    // The other project sees its own claim.
    reply(&f, &[[1.0, 0.0]]);
    assert_eq!(
        ids(&search(&f, &cases, &b, "automobile", 50, None)),
        [other]
    );
    let jobs = vector_jobs(&f);
    assert_eq!(
        (jobs.completed, jobs.ready, jobs.failed),
        (3, 0, 0),
        "{jobs:?}"
    );
}

#[test]
fn without_an_embedding_route_search_is_lexical_and_diagnostics_say_why() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let human = principal(f.store.as_ref(), "user-a", PrincipalKind::Human);
    let a = project(&human, "knowledge-a");
    // The shared runtime exists, but no native embedding route is installed.
    let index = Arc::new(IndexService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    let (_service, cases) = knowledge(&f, Some(index));
    let claim = record(&f, &cases, &a, "far", "kitchen recipe");
    assert_eq!(
        vector_jobs(&f),
        QueueStatus::default(),
        "nothing to embed without a route"
    );
    assert_eq!(ids(&search(&f, &cases, &a, "kitchen", 50, None)), [claim]);
    assert!(
        search(&f, &cases, &a, "automobile", 50, None)
            .items
            .0
            .is_empty()
    );
    assert_eq!(sent(&f), 0, "no embedding request is sent");
    let diagnostic = retrieval(&f, &cases, &a);
    assert_eq!(diagnostic.code.0, "LexicalOnly");
    assert!(
        diagnostic.message.0.contains("(NoNativeEmbeddingRuntime)"),
        "{}",
        diagnostic.message.0
    );
}
