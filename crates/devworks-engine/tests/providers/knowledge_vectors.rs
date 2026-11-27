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
        DomainErrorCode, OperationState, ProjectId,
        records::{Diagnostic, KnowledgeRetrievalMode, KnowledgeSearchPage, KnowledgeSearchResult},
        results::{DomainQueryReply, OperationResult},
    },
    services::{
        UseCases,
        conventions::indexing::IndexService,
        knowledge::{KnowledgeService, VECTOR_CONSUMER},
    },
};
use devworks_engine::knowledge::claim_vector_consumer;
use devworks_test_support::seed;

/// One recorded embedding reply, for the next request that has none: a vector per input, in
/// input order.
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
        .replies
        .push_back(body.to_string().into_bytes());
}

fn project(ctx: &InvocationContext, name: &str) -> InvocationContext {
    ctx.clone()
        .with_lineage(Some(ProjectId(id(name))), None, None)
}

fn scope(ctx: &InvocationContext) -> Value {
    json!({"kind": "Project", "projectId": ctx.project_id()})
}

/// Records one claim about the entity "Garage" saying `value`, returning its id.
fn record(f: &Fx, cases: &UseCases, ctx: &InvocationContext, key: &str, value: &str) -> Id {
    record_about(f, cases, ctx, key, "Garage", &[value]).remove(0)
}

/// Records claims about the entity `subject`, one saying each of `values`, returning their ids.
fn record_about(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    key: &str,
    subject: &str,
    values: &[&str],
) -> Vec<Id> {
    let claims: Vec<Value> = values
        .iter()
        .map(|value| {
            json!({"kind": "Fact", "subjectId": "s", "predicate": "describes",
                "value": value, "confidence": 0.7, "evidenceIds": [], "tags": []})
        })
        .collect();
    let result = command(
        f,
        cases,
        ctx,
        "knowledge.record",
        json!({"protocol": "1.0-draft.1", "mode": "Command", "requestKey": key,
            "schemaRevision": 2, "body": {"kind": "knowledge.record", "scope": scope(ctx),
            "batch": {"entities": [{"clientId": "s", "label": subject, "entityType": "topic",
                "aliases": []}], "evidence": [], "claims": claims}}}),
    );
    let OperationResult::KnowledgeRecord { value } = result else {
        panic!("{result:?}")
    };
    value.claim_ids.0
}

/// Every page of `ctx`'s search for `text`, `limit` claims each, a query vector `near` recorded
/// for each page; the claims listed and whether the first page was work-limited.
fn every_page(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    text: &str,
    limit: u32,
    near: [f32; 2],
) -> (Vec<Id>, bool) {
    let (mut listed, mut limited) = (Vec::new(), None);
    let mut cursor: Option<String> = None;
    loop {
        reply(f, &[near]);
        let page = search(f, cases, ctx, text, limit, cursor.as_deref());
        assert!(matches!(page.mode, KnowledgeRetrievalMode::Hybrid));
        limited.get_or_insert(page.work_limited);
        listed.extend(ids(&page));
        match page.next_cursor {
            Some(c) => cursor = Some(c.0),
            None => return (listed, limited.unwrap()),
        }
    }
}

fn expired(reply: DomainQueryReply) {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::CursorExpired)
        }
        other => panic!("{other:?}"),
    }
}

fn query_at(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    op: &str,
    body: Value,
    revision: u32,
) -> DomainQueryReply {
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
        "schemaRevision": revision, "body": body});
    cases
        .query(&gateway, ctx, op, frame.to_string().as_bytes())
        .unwrap()
}

fn query(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    op: &str,
    body: Value,
) -> OperationResult {
    match query_at(f, cases, ctx, op, body, 2) {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("{other:?}"),
    }
}

/// One `knowledge.search` reply at the revision that reports the retrieval mode.
fn search_reply(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    text: &str,
    limit: u32,
    cursor: Option<&str>,
) -> DomainQueryReply {
    let mut page = json!({"limit": limit});
    if let Some(cursor) = cursor {
        page["cursor"] = json!(cursor);
    }
    let body = json!({"scope": scope(ctx), "text": text, "tags": [], "kinds": [],
        "page": page, "entityLimit": 50, "claimLimit": 50});
    query_at(f, cases, ctx, "knowledge.search", body, 30)
}

fn search(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    text: &str,
    limit: u32,
    cursor: Option<&str>,
) -> KnowledgeSearchPage {
    match search_reply(f, cases, ctx, text, limit, cursor) {
        DomainQueryReply::Result {
            result:
                OperationResult::KnowledgeSearch {
                    value: KnowledgeSearchResult::Current(page),
                },
            ..
        } => page,
        other => panic!("{other:?}"),
    }
}

fn ids(page: &KnowledgeSearchPage) -> Vec<Id> {
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

/// One recorded reply for each batch of 32 claims a delivery embeds of `n` pending claims,
/// each claim's vector near [1, 0] and distinct from every other.
fn reply_batches(f: &Fx, n: usize) {
    for start in (0..n).step_by(32) {
        let vectors: Vec<[f32; 2]> = (start..n.min(start + 32))
            .map(|k| [1.0, k as f32 / 1000.0])
            .collect();
        reply(f, &vectors);
    }
}

/// How many claims of `ctx`'s project have no vector in the selected space yet, up to 32.
fn pending(f: &Fx, index: &IndexService, ctx: &InvocationContext) -> usize {
    let space = read(f.store.as_ref(), |r| index.configuration_read(r, ctx))
        .space
        .unwrap()
        .id;
    let space = Id::new(space).unwrap();
    let scope =
        devworks_core::ports::storage::KnowledgeScope::Project(ctx.project_id().unwrap().clone());
    read(f.store.as_ref(), move |r| {
        r.knowledge()
            .pending_knowledge_claim_vectors(&scope, &space, 32)
    })
    .len()
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

    assert!(matches!(found.mode, KnowledgeRetrievalMode::Hybrid));
    assert!(!found.work_limited);

    // The word match and the meaning match are fused; the cursor resumes across them, and
    // each is listed once.
    reply(&f, &[[1.0, 0.0]]);
    let first = search(&f, &cases, &a, "kitchen", 1, None);
    reply(&f, &[[1.0, 0.0]]);
    let second = search(
        &f,
        &cases,
        &a,
        "kitchen",
        1,
        first.next_cursor.as_ref().map(|c| c.0.as_str()),
    );
    assert!(second.next_cursor.is_none());
    let mut both = ids(&first);
    both.extend(ids(&second));
    both.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    let mut want = vec![far, near];
    want.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(both, want);

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
fn a_claim_found_by_words_and_by_meaning_ranks_above_claims_found_by_one() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    // The best word match points away from the query's meaning ...
    let words = record(&f, &cases, &a, "words", "kitchen kitchen kitchen");
    reply(&f, &[[-1.0, 0.1]]);
    embed(&f, &service);
    // ... the nearest claim shares no word ...
    let meaning = record(&f, &cases, &a, "meaning", "a vehicle parked outside");
    reply(&f, &[[1.0, 0.0]]);
    embed(&f, &service);
    // ... and the second of each is found both ways.
    let both = record(&f, &cases, &a, "both", "kitchen recipe");
    reply(&f, &[[0.8, 0.2]]);
    embed(&f, &service);
    reply(&f, &[[1.0, 0.0]]);
    let page = search(&f, &cases, &a, "kitchen", 50, None);
    assert!(matches!(page.mode, KnowledgeRetrievalMode::Hybrid));
    let found = ids(&page);
    assert_eq!(found[0], both, "{found:?}");
    let mut rest = found[1..].to_vec();
    rest.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    let mut want = vec![words, meaning];
    want.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(rest, want);
    // Pages of one list the same claims, each once, in the same order.
    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        reply(&f, &[[1.0, 0.0]]);
        let page = search(&f, &cases, &a, "kitchen", 1, cursor.as_deref());
        paged.extend(ids(&page));
        match page.next_cursor {
            Some(c) => cursor = Some(c.0),
            None => break,
        }
    }
    assert_eq!(paged, found);
}

#[test]
fn a_vector_stored_between_pages_expires_a_fused_cursor() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    record(&f, &cases, &a, "first", "kitchen recipe");
    reply(&f, &[[0.9, 0.1]]);
    embed(&f, &service);
    record(&f, &cases, &a, "second", "kitchen knife");
    // The second claim's vector is not stored yet when the first page is read.
    reply(&f, &[[1.0, 0.0]]);
    let page = search(&f, &cases, &a, "kitchen", 1, None);
    assert!(page.next_cursor.is_some());
    // Storing a vector does not advance the scope's revision, but it changes the meaning
    // source: the cursor expires instead of repeating or skipping a claim.
    reply(&f, &[[0.8, 0.2]]);
    embed(&f, &service);
    reply(&f, &[[1.0, 0.0]]);
    match search_reply(
        &f,
        &cases,
        &a,
        "kitchen",
        1,
        page.next_cursor.as_ref().map(|c| c.0.as_str()),
    ) {
        DomainQueryReply::Rejected { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::CursorExpired)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_word_match_found_through_its_subject_ranks_below_a_word_and_meaning_match() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    // The best word match is about the entity the query names, so the graph source lists it
    // too; its meaning is far from the query's.
    let subject =
        record_about(&f, &cases, &a, "subject", "Kitchen", &["kitchen kitchen"]).remove(0);
    reply(&f, &[[-1.0, 0.1]]);
    embed(&f, &service);
    let meaning = record(&f, &cases, &a, "meaning", "a vehicle parked outside");
    reply(&f, &[[1.0, 0.0]]);
    embed(&f, &service);
    let both = record(&f, &cases, &a, "both", "kitchen recipe");
    reply(&f, &[[0.8, 0.2]]);
    embed(&f, &service);
    reply(&f, &[[1.0, 0.0]]);
    let found = ids(&search(&f, &cases, &a, "kitchen", 50, None));
    assert_eq!(found, [both, subject, meaning]);
}

#[test]
fn every_meaning_match_past_the_cap_is_listed_once_after_the_fused_head() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index.clone()));
    let n = usize::from(devworks_core::services::knowledge::CANDIDATES) + 1;
    let texts: Vec<String> = (0..n).map(|i| format!("plain note {i}")).collect();
    let mut all = Vec::new();
    for (i, chunk) in texts.chunks(100).enumerate() {
        let values: Vec<&str> = chunk.iter().map(String::as_str).collect();
        all.extend(record_about(
            &f,
            &cases,
            &a,
            &format!("notes-{i}"),
            "Garage",
            &values,
        ));
    }
    // The first delivery embeds every claim of the scope, batch by batch; the later ones find
    // none pending.
    reply_batches(&f, n);
    embed(&f, &service);
    assert_eq!(pending(&f, &index, &a), 0);
    let jobs = vector_jobs(&f);
    assert_eq!((jobs.ready, jobs.failed), (0, 0), "{jobs:?}");
    // No claim shares the query's word: every one is a meaning match only.
    let (listed, limited) = every_page(&f, &cases, &a, "zebra", 50, [1.0, 0.0]);
    assert!(limited, "the meaning source is past its cap");
    assert_eq!(listed.len(), n);
    let mut sorted = listed.clone();
    sorted.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    sorted.dedup();
    all.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(sorted, all, "every claim exactly once");
}

#[test]
fn a_lexical_listing_stays_lexical_and_another_query_vector_expires_a_hybrid_one() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    let first = record(&f, &cases, &a, "first", "kitchen recipe");
    reply(&f, &[[0.9, 0.1]]);
    embed(&f, &service);
    let second = record(&f, &cases, &a, "second", "kitchen knife");
    reply(&f, &[[0.1, 0.9]]);
    embed(&f, &service);
    // No reply is recorded: the route fails and the first page is lexical.
    let page = search(&f, &cases, &a, "kitchen", 1, None);
    assert!(matches!(page.mode, KnowledgeRetrievalMode::LexicalOnly));
    // The provider recovers, but the listing stays lexical and asks for no embedding.
    reply(&f, &[[1.0, 0.0]]);
    let before = sent(&f);
    let rest = search(
        &f,
        &cases,
        &a,
        "kitchen",
        1,
        page.next_cursor.as_ref().map(|c| c.0.as_str()),
    );
    assert_eq!(sent(&f), before, "a lexical listing embeds no query");
    assert!(matches!(rest.mode, KnowledgeRetrievalMode::LexicalOnly));
    let mut listed = ids(&page);
    listed.extend(ids(&rest));
    listed.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    let mut want = vec![first, second];
    want.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(listed, want);
    assert!(rest.next_cursor.is_none());
    // A hybrid listing (the recorded reply above) whose next query vector differs expires.
    let page = search(&f, &cases, &a, "kitchen", 1, None);
    assert!(matches!(page.mode, KnowledgeRetrievalMode::Hybrid));
    reply(&f, &[[0.9, 0.1]]);
    expired(search_reply(
        &f,
        &cases,
        &a,
        "kitchen",
        1,
        page.next_cursor.as_ref().map(|c| c.0.as_str()),
    ));
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
    let page = search(&f, &cases, &a, "kitchen", 50, None);
    assert_eq!(ids(&page), [claim]);
    assert!(matches!(page.mode, KnowledgeRetrievalMode::LexicalOnly));
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

/// The captured embedding inputs of every claim batch the producer journaled.
fn claim_inputs(f: &Fx) -> Vec<ArtifactRef> {
    read(f.store.as_ref(), |r| {
        let prepared = r.operations().operations_for_capability(
            "conventions.embedding_prepare",
            &[OperationState::ObservedComplete],
            None,
            50,
        )?;
        let mut refs = Vec::new();
        for op in prepared
            .into_iter()
            .filter(|op| op.payload["origin"]["kind"] == "KnowledgeIndex")
        {
            let request: Id = serde_json::from_value(op.payload["requestId"].clone()).unwrap();
            let row = r
                .conventions()
                .convention_embedding_inputs(&request)?
                .unwrap();
            for input in row.input_refs {
                refs.push(serde_json::from_value(input).unwrap());
            }
        }
        Ok(refs)
    })
}

/// Every artifact the user can list in `project` or the workspace whose current bytes hold
/// `text`.
fn listed_with(f: &Fx, ctx: &InvocationContext, project: &str, text: &str) -> usize {
    let listed = read(f.store.as_ref(), |r| {
        let mut all = r
            .artifacts()
            .list(&ProjectId(id(project)), None, &[], None, 500)?;
        all.extend(r.artifacts().list_workspace(None, &[], None, 500)?);
        let mut refs = Vec::new();
        for artifact in all {
            if let Some(version) = artifact.current_version_id.clone()
                && let Some(v) = r.artifacts().version(&version)?
            {
                refs.push(devworks_core::services::artifacts::version_ref(&v));
            }
        }
        Ok(refs)
    });
    listed
        .iter()
        .filter(|reference| {
            f.artifacts
                .read_exact(f.store.as_ref(), ctx, reference, 1 << 20)
                .is_ok_and(|(bytes, _)| String::from_utf8_lossy(&bytes).contains(text))
        })
        .count()
}

#[test]
fn a_deleted_claims_text_is_in_no_listed_or_readable_artifact() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    let text = "a private vehicle parked outside";
    let claim = record(&f, &cases, &a, "private", text);
    reply(&f, &[[0.9, 0.1]]);
    embed(&f, &service);
    let inputs = claim_inputs(&f);
    assert_eq!(inputs.len(), 1);
    // While the claim is live its captured input is readable by its owner, never listed.
    let (bytes, provenance) = f
        .artifacts
        .read_exact(f.store.as_ref(), &a, &inputs[0], 4096)
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains(text));
    assert_eq!(provenance["derived"]["kind"], "KnowledgeEmbeddingInput");
    assert_eq!(provenance["derived"]["knowledgeClaimId"], json!(claim));
    assert_eq!(listed_with(&f, &a, "knowledge-a", text), 0);
    let c = claim.clone();
    let revision = read(f.store.as_ref(), |r| r.knowledge().knowledge_claim(&c))
        .unwrap()
        .revision;
    let result = command(
        &f,
        &cases,
        &a,
        "knowledge.delete_selected",
        json!({"protocol": "1.0-draft.1", "mode": "Command", "requestKey": "delete",
            "schemaRevision": 1, "body": {"kind": "knowledge.delete_selected",
            "scope": scope(&a), "claims": [{"id": claim, "revision": revision.to_string()}]}}),
    );
    assert!(matches!(
        result,
        OperationResult::KnowledgeDeleteSelected { .. }
    ));
    // The input's version is retired with the claim: unreadable, and in no listing.
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &a, &inputs[0], 4096)
            .is_err()
    );
    let retired = read(f.store.as_ref(), |r| {
        r.artifacts().version(&inputs[0].version_id)
    })
    .unwrap();
    assert!(!retired.live());
    assert_eq!(listed_with(&f, &a, "knowledge-a", text), 0);
}

#[test]
fn claims_recorded_before_a_space_is_selected_get_vectors_when_it_is() {
    use devworks_core::protocol::records::RouterConfigAction;
    use devworks_core::services::conventions::indexing::SelectionListener;
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, native, spaces) = super::convention_search::qualified_candidate(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let index = Arc::new(IndexService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    index.set_embeddings(Some(native));
    let (service, cases) = knowledge(&f, Some(index.clone()));
    index
        .add_selection_listener(Arc::downgrade(&service) as std::sync::Weak<dyn SelectionListener>);
    assert!(spaces.install_selection_indexer(Arc::downgrade(&index)));
    let claim = record(&f, &cases, &a, "before", "a vehicle parked outside");
    assert_eq!(
        vector_jobs(&f),
        QueueStatus::default(),
        "no space is selected yet"
    );
    // The user selects the qualified space through the shared configuration operation.
    let qualification = read(f.store.as_ref(), |r| {
        r.router().embedding_qualification(&id("actual-candidate"))
    })
    .unwrap();
    let action = RouterConfigAction::SelectEmbeddingSpace {
        expected_revision: Revision::new(qualification.revision).unwrap(),
        expected_selection_revision: Revision::new(1).unwrap(),
        candidate_id: id("actual-candidate"),
    };
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "schemaRevision": 3,
        "requestKey": "select-space", "body": {"kind": "router.configure", "action": action}});
    let gateway = Gateway {
        authority: &Authority,
        store: &*f.store,
        ids: f.h.ids.as_ref(),
    };
    gateway
        .command(
            &human,
            "router.configure",
            frame.to_string().as_bytes(),
            spaces.clone(),
        )
        .unwrap();
    // Selected, not embedded yet: still word search, and diagnostics say why.
    let diagnostic = retrieval(&f, &cases, &a);
    assert_eq!(diagnostic.code.0, "LexicalOnly");
    assert!(
        diagnostic.message.0.contains("(NoClaimVectors)"),
        "{}",
        diagnostic.message.0
    );
    // The selection queued the scope's delivery: no other write is needed.
    reply(&f, &[[0.9, 0.1]]);
    embed(&f, &service);
    reply(&f, &[[1.0, 0.0]]);
    assert_eq!(
        ids(&search(&f, &cases, &a, "automobile", 50, None)),
        [claim]
    );
    assert_eq!(retrieval(&f, &cases, &a).code.0, "Hybrid");
}

#[test]
fn a_route_failure_at_query_time_keeps_the_word_matches_and_expires_a_fused_cursor() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    let first = record(&f, &cases, &a, "first", "kitchen recipe");
    reply(&f, &[[0.0, 1.0]]);
    embed(&f, &service);
    let second = record(&f, &cases, &a, "second", "kitchen knife");
    reply(&f, &[[0.1, 0.9]]);
    embed(&f, &service);
    reply(&f, &[[1.0, 0.0]]);
    let page = search(&f, &cases, &a, "kitchen", 1, None);
    assert_eq!(page.items.0.len(), 1);
    assert!(matches!(page.mode, KnowledgeRetrievalMode::Hybrid));
    // No reply is recorded for the next query embedding: the route fails at query time. The
    // fused order without meaning matches is another order, so the cursor expires instead of
    // repeating or skipping a claim.
    match search_reply(
        &f,
        &cases,
        &a,
        "kitchen",
        1,
        page.next_cursor.as_ref().map(|c| c.0.as_str()),
    ) {
        DomainQueryReply::Rejected { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::CursorExpired)
        }
        other => panic!("{other:?}"),
    }
    // A first page without a query vector lists the word matches and says it is lexical.
    let lexical = search(&f, &cases, &a, "kitchen", 50, None);
    let mut all = ids(&lexical);
    all.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    let mut want = vec![first, second];
    want.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(all, want);
    assert!(matches!(lexical.mode, KnowledgeRetrievalMode::LexicalOnly));
}

#[test]
fn an_unusable_provider_vector_is_recorded_and_the_batch_kept() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index));
    // Two claims of one scope in one batch: the first gets a zero vector.
    record(&f, &cases, &a, "zero", "kitchen recipe");
    record(&f, &cases, &a, "good", "a vehicle parked outside");
    reply(&f, &[[0.0, 0.0], [0.9, 0.1]]);
    embed(&f, &service);
    let jobs = vector_jobs(&f);
    assert_eq!(jobs.failed, 0, "{jobs:?}");
    let diagnostic = retrieval(&f, &cases, &a);
    assert_eq!(diagnostic.code.0, "Hybrid");
    assert!(
        diagnostic.message.0.contains("1 of 2 claims"),
        "{}",
        diagnostic.message.0
    );
    // The recorded text is not embedded again by a later delivery of the scope.
    let before = sent(&f);
    record(&f, &cases, &a, "third", "a parked truck");
    reply(&f, &[[0.8, 0.2]]);
    embed(&f, &service);
    assert_eq!(sent(&f) - before, 1, "only the new claim is embedded");
}

#[test]
fn one_delivery_embeds_every_batch_of_a_scope_and_every_claim_gets_its_vector() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, _native, index) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[seed::ProjectSeed::new("knowledge-a")],
        &[],
    )
    .unwrap();
    let a = project(&human, "knowledge-a");
    let (service, cases) = knowledge(&f, Some(index.clone()));
    let texts: Vec<String> = (0..70).map(|i| format!("plain note {i}")).collect();
    let values: Vec<&str> = texts.iter().map(String::as_str).collect();
    record_about(&f, &cases, &a, "notes", "Garage", &values);
    // Two full batches and the rest, each its own request with its own recorded reply.
    reply_batches(&f, 70);
    let before = sent(&f);
    embed(&f, &service);
    let inputs: Vec<usize> = f.http.lock().unwrap().requests.lock().unwrap()[before..]
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
            body["input"].as_array().unwrap().len()
        })
        .collect();
    assert_eq!(inputs, [32, 32, 6], "one request per batch of at most 32");
    let jobs = vector_jobs(&f);
    assert_eq!(
        (jobs.completed, jobs.ready, jobs.failed),
        (1, 0, 0),
        "{jobs:?}"
    );
    assert_eq!(pending(&f, &index, &a), 0);
    let diagnostic = retrieval(&f, &cases, &a);
    assert_eq!(diagnostic.code.0, "Hybrid");
    assert!(
        diagnostic.message.0.contains("70 of 70 claims"),
        "{}",
        diagnostic.message.0
    );
}
