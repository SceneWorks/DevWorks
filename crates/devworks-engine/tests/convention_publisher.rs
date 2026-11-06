//! Actual authored-body publication over SQLite and managed immutable files; no model runs.
mod common;
use common::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::{
    context::{InvocationContext, PrincipalKind},
    ports::storage::*,
    protocol::{OperationId, OperationState},
    services::{
        artifacts::{ArtifactService, version_ref},
        conventions::indexing::{IndexService, publisher::IndexOutcome},
    },
};
use serde_json::json;
use std::sync::Arc;
struct Fixture {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    index: Arc<IndexService>,
    ctx: InvocationContext,
}
impl Fixture {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        let ctx = principal(&*store, "author", PrincipalKind::Human);
        let (artifacts, index) = services(&h, store.clone());
        Self {
            h,
            store,
            artifacts,
            index,
            ctx,
        }
    }
    fn source(&self, name: &str, body: &str) -> (StoredConvention, OperationRecord) {
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                body.as_bytes(),
                json!({"fixture":"authored body"}),
            )
            .unwrap()
            .unwrap();
        let (service, index, ctx, pending, name) = (
            self.artifacts.clone(),
            self.index.clone(),
            self.ctx.clone(),
            staged.clone(),
            name.to_owned(),
        );
        let row = write(&*self.store, move |tx| {
            let v = service.commit_host_bytes(
                tx,
                &pending,
                "authored body",
                "text/markdown",
                json!({"fixture":"body"}),
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
                content_ref: Some(json!(version_ref(&v))),
                raw_content_ref: None,
                applicability: vec![],
                provenance: json!({"kind":"Authored","actorId":ctx.principal()}),
                author_id: ctx.principal().clone(),
                created_at_ms: tx.now_ms(),
                updated_at_ms: tx.now_ms(),
                legacy_approved: false,
            };
            tx.conventions().create_convention(&row)?;
            let operation = index.record_changed(tx, &ctx, &row)?;
            Ok((row, operation))
        })
        .unwrap();
        self.artifacts
            .settle_host_bytes(&*self.store, &staged, true);
        let operation = dispatch(&*self.store, &row.1);
        (row.0, operation)
    }
}
fn services(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
) -> (Arc<ArtifactService>, Arc<IndexService>) {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let index = Arc::new(IndexService::new(
        store,
        artifacts.clone(),
        h.ids.clone(),
        h.clock(),
    ));
    (artifacts, index)
}
fn dispatch(store: &dyn ApplicationStore, id: &OperationId) -> OperationRecord {
    let id = id.clone();
    write(store, move |tx| {
        let op = tx.operations().operation(&id)?.unwrap();
        let a = tx
            .operations()
            .begin_attempt(&id, op.revision, AttemptBinding::default())?;
        let op = tx.operations().operation(&id)?.unwrap();
        tx.operations().transition(
            &id,
            op.revision,
            Some(&a.fence()),
            OperationState::Dispatching,
            None,
        )
    })
    .unwrap()
}
#[test]
fn accepted_body_has_queued_metadata_and_publishes_complete_lexical_terms_once() {
    let f = Fixture::new();
    let body = format!(
        "---\nignored: metadata\n---\n# Consumer λ\n{}\n## Tail\nneedle at the end",
        (0..2500)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let (owner, op) = f.source("rules", &body);
    let queued = read(&*f.store, |s| {
        s.conventions().convention_index_status(&owner.id)
    })
    .unwrap();
    assert_eq!(queued.status, "Queued");
    assert!(queued.embedding_space.is_none());
    let IndexOutcome::Completed(completion) =
        f.index.run_index(&op, &Cancellation::new(), None).unwrap()
    else {
        panic!("complete authored index")
    };
    let (head, count) = read(&*f.store, |s| {
        let head = s.conventions().convention_index_head(&owner.id)?.unwrap();
        let count = s
            .conventions()
            .convention_chunk_count(&head.generation_id)?;
        Ok((head, count))
    });
    assert_eq!(head.status, "LexicalOnly");
    assert!(count > 4);
    assert_eq!(
        completion["indexed"], count,
        "retained ingestion result counts chunks rather than pages"
    );
    assert_eq!(completion["conventions"], 1);
    assert!(head.error.is_some());
    let stats = read(&*f.store, |s| {
        s.conventions().convention_lexical_statistics(
            &ConventionQuery {
                scopes: vec![KnowledgeScope::Workspace],
                ..Default::default()
            },
            &["needle".into(), "guide".into()],
        )
    });
    assert_eq!(stats.documents, count);
    assert_eq!(
        stats
            .document_frequencies
            .iter()
            .find(|t| t.term == "needle")
            .unwrap()
            .frequency,
        1
    );
    let replay = f.index.run_index(&op, &Cancellation::new(), None).unwrap();
    assert!(matches!(replay, IndexOutcome::Completed(_)));
    assert_eq!(
        read(&*f.store, |s| s
            .conventions()
            .convention_chunk_count(&head.generation_id)),
        count
    );
}
#[test]
fn interrupted_boot_catches_up_original_operation_and_keeps_exact_source() {
    let f = Fixture::new();
    let (owner, op) = f.source("restart", "# A\nalpha\n## B\nbeta");
    let Fixture {
        h,
        store,
        artifacts,
        index,
        ctx: _,
    } = f;
    drop(index);
    drop(artifacts);
    drop(store);
    let store = h.open();
    write(&*store, |tx| tx.operations().classify_interrupted(500)).unwrap();
    let op = read(&*store, |s| s.operations().operation(&op.id)).unwrap();
    assert_eq!(op.state, OperationState::OutcomeUnknown);
    let (_, index) = services(&h, store.clone());
    assert!(matches!(
        index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
    let head = read(&*store, |s| {
        s.conventions().convention_index_head(&owner.id)
    })
    .unwrap();
    assert_eq!(head.source_ref, owner.content_ref.unwrap());
    assert_eq!(head.convention_revision, 1);
}
#[test]
fn cancellation_and_shutdown_never_publish_a_partial_head() {
    let f = Fixture::new();
    let (owner, op) = f.source("cancel", "# Heading\nbody");
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(matches!(
        f.index.run_index(&op, &cancel, None),
        Err(StorageError::Cancelled)
    ));
    assert!(
        read(&*f.store, |s| s
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
    f.index.stop();
    assert!(matches!(
        f.index.run_index(&op, &Cancellation::new(), None),
        Err(StorageError::Cancelled)
    ));
    assert!(
        read(&*f.store, |s| s
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
}
#[test]
fn source_revision_change_fences_old_queued_work() {
    let f = Fixture::new();
    let (mut owner, op) = f.source("stale", "body");
    owner.revision = 2;
    owner.title = "new title".into();
    let changed = owner.clone();
    write(&*f.store, move |tx| {
        tx.conventions().revise_convention(&changed, 1)
    })
    .unwrap();
    assert!(f.index.run_index(&op, &Cancellation::new(), None).is_err());
    assert!(
        read(&*f.store, |s| s
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
}
#[test]
fn retired_source_is_not_reembedded_or_made_searchable() {
    let f = Fixture::new();
    let (mut owner, _) = f.source("retired", "full body");
    owner.revision = 2;
    owner.lifecycle = "Retired".into();
    let (changed, index, ctx) = (owner.clone(), f.index.clone(), f.ctx.clone());
    let id = write(&*f.store, move |tx| {
        tx.conventions().revise_convention(&changed, 1)?;
        index.record_changed(tx, &ctx, &changed)
    })
    .unwrap();
    let op = dispatch(&*f.store, &id);
    assert!(matches!(
        f.index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
    assert!(
        read(&*f.store, |s| s
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
}

// A thin native gateway adapter to the SAME core owner, while the operation family composes.
struct Search(Arc<IndexService>);
fn search_input(
    q: &devworks_core::gateway::AdmittedQuery,
) -> &devworks_core::operations::conventions::SearchInput {
    match q.input() {
        devworks_core::operations::OperationInput::Conventions(
            devworks_core::operations::conventions::Input::Search(input),
        ) => input,
        _ => panic!("selected actual search input"),
    }
}
fn fault(
    error: devworks_core::services::sessions::Failure,
) -> devworks_core::protocol::DomainFault {
    use devworks_core::{
        protocol::{CorrelationId, DomainErrorCode},
        services::sessions::{Failure, Refusal},
    };
    let refusal = match error {
        Failure::Refused(r) => r,
        Failure::Storage(StorageError::CursorExpired) => {
            Refusal::new(DomainErrorCode::CursorExpired, "search", None)
        }
        Failure::Storage(StorageError::Cancelled) => {
            Refusal::new(DomainErrorCode::Cancelled, "search", None)
        }
        Failure::Storage(e) => panic!("unexpected storage fault: {e:?}"),
    };
    refusal.fault(CorrelationId(id("search-fault")))
}
impl devworks_core::ports::QueryHandler for Search {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        q: &devworks_core::gateway::AdmittedQuery,
        env: &devworks_core::ports::Preparation<'_>,
    ) -> Result<Option<devworks_core::ports::Prepared>, devworks_core::protocol::DomainFault> {
        self.0
            .prepare_search(ctx, q, env, search_input(q))
            .map(|p| Some(devworks_core::ports::Prepared::new(p)))
            .map_err(fault)
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        q: &devworks_core::gateway::AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<devworks_core::ports::QueryAnswer, devworks_core::protocol::DomainFault> {
        let result = self
            .0
            .search(
                read,
                ctx,
                q,
                search_input(q),
                q.prepared().expect("real preparation"),
            )
            .map_err(fault)?;
        Ok(devworks_core::ports::QueryAnswer {
            stamp: read
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), result.scope_revision.clone()),
            result: devworks_core::protocol::results::OperationResult::ConventionsSearch {
                value: devworks_core::protocol::records::ConventionSearchPageResult::Current(
                    result,
                ),
            },
        })
    }
}
fn search(f: &Fixture, query: &str, cursor: Option<&str>, top_k: i64) -> serde_json::Value {
    let gateway = devworks_core::gateway::Gateway {
        authority: &Authority,
        store: &*f.store,
        ids: f.h.ids.as_ref(),
    };
    let mut body = json!({"kind":"conventions.search","scope":{"kind":"Workspace"},"page":{"limit":1},"applicability":[],"text":"alpha","topK":top_k.to_string()});
    if let Some(cursor) = cursor {
        body["page"]["cursor"] = json!(cursor);
    }
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":query,"schemaRevision":3,"body":body});
    serde_json::to_value(
        gateway
            .query(
                &f.ctx,
                "conventions.search",
                frame.to_string().as_bytes(),
                &Search(f.index.clone()),
            )
            .unwrap(),
    )
    .unwrap()
}
#[test]
fn actual_gateway_search_ranks_full_corpus_and_pages_the_same_bounded_top_k() {
    let f = Fixture::new();
    for name in ["a", "b", "c", "d"] {
        let (_, op) = f.source(name, "# Policy\nalpha authoritative rule");
        f.index.run_index(&op, &Cancellation::new(), None).unwrap();
    }
    let first = search(&f, "query-first", None, 3);
    let page = &first["result"]["value"];
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "{first}");
    assert_eq!(page["retrieval"]["mode"], "Lexical");
    assert!(
        page["items"][0]["sectionPath"]["text"]
            .as_str()
            .unwrap()
            .contains("Policy")
    );
    assert_eq!(page["items"][0]["convention"]["id"], "a");
    let cursor = page["nextCursor"].as_str().unwrap();
    let second = search(&f, "query-second", Some(cursor), 3);
    let page = &second["result"]["value"];
    assert_eq!(page["items"][0]["convention"]["id"], "b");
    let cursor = page["nextCursor"].as_str().unwrap();
    let third = search(&f, "query-third", Some(cursor), 3);
    assert_eq!(
        third["result"]["value"]["items"][0]["convention"]["id"],
        "c"
    );
    assert_eq!(third["result"]["value"]["hasMore"], false);
}
#[test]
fn search_cursor_expires_on_changed_source_and_changed_top_k() {
    let f = Fixture::new();
    let (mut owner, op) = f.source("a", "alpha");
    f.index.run_index(&op, &Cancellation::new(), None).unwrap();
    let (_, op) = f.source("b", "alpha");
    f.index.run_index(&op, &Cancellation::new(), None).unwrap();
    let first = search(&f, "query-first", None, 2);
    let cursor = first["result"]["value"]["nextCursor"].as_str().unwrap();
    assert_eq!(
        search(&f, "query-changed-k", Some(cursor), 3)["fault"]["code"],
        "CursorExpired"
    );
    owner.revision += 1;
    owner.title = "changed title".into();
    write(&*f.store, move |tx| {
        tx.conventions().revise_convention(&owner, 1)
    })
    .unwrap();
    assert_eq!(
        search(&f, "query-changed-source", Some(cursor), 2)["fault"]["code"],
        "CursorExpired"
    );
}
struct IndexCommand(Arc<IndexService>);
impl devworks_core::ports::CommandHandler for IndexCommand {
    fn decide(
        &self,
        ctx: &InvocationContext,
        c: &devworks_core::gateway::AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<devworks_core::ports::Decided, devworks_core::ports::HandlerFailure> {
        let devworks_core::operations::OperationInput::Conventions(
            devworks_core::operations::conventions::Input::Index(input),
        ) = c.input()
        else {
            panic!("actual selected index input")
        };
        self.0.decide_index(ctx, c, tx, input).map_err(|e| match e {
            devworks_core::services::sessions::Failure::Storage(e) => e.into(),
            e => panic!("unexpected actual owner refusal {e:?}"),
        })
    }
}
#[test]
fn actual_accepted_all_request_captures_configuration_and_processes_every_keyset_page() {
    let f = Fixture::new();
    let mut owners = Vec::new();
    for n in 0..65 {
        owners.push(f.source(&format!("guide-{n:03}"), "alpha rule").0);
    }
    let revision = read(&*f.store, |s| {
        s.conventions()
            .convention_scope_revision(&KnowledgeScope::Workspace)
    });
    let gateway = devworks_core::gateway::Gateway {
        authority: &Authority,
        store: &*f.store,
        ids: f.h.ids.as_ref(),
    };
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"all-index","schemaRevision":3,"body":{"kind":"conventions.index","allScope":{"kind":"Workspace"},"expectedRevision":revision.to_string()}});
    let handler = Arc::new(IndexCommand(f.index.clone()));
    let first = gateway
        .command(
            &f.ctx,
            "conventions.index",
            frame.to_string().as_bytes(),
            handler.clone(),
        )
        .unwrap();
    let replay = gateway
        .command(
            &f.ctx,
            "conventions.index",
            frame.to_string().as_bytes(),
            handler,
        )
        .unwrap();
    let devworks_core::protocol::CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = first
    else {
        panic!("actual durable Accepted")
    };
    assert!(matches!(
        receipt.decision,
        devworks_core::protocol::results::DomainDecision::Accepted { .. }
    ));
    assert!(matches!(
        replay,
        devworks_core::protocol::CommandReply::Recorded { replayed: true, .. }
    ));
    let operation = receipt.operation_id.unwrap();
    let before = read(&*f.store, |s| s.operations().operation(&operation)).unwrap();
    assert_eq!(
        before.payload["configurationSourceDigest"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(before.payload["embeddingSpace"].is_null());
    let op = dispatch(&*f.store, &operation);
    let IndexOutcome::Completed(result) =
        f.index.run_index(&op, &Cancellation::new(), None).unwrap()
    else {
        panic!("complete full scope")
    };
    assert_eq!(result["indexed"], 65);
    for owner in &owners {
        let head = read(&*f.store, |s| {
            s.conventions().convention_index_head(&owner.id)
        })
        .unwrap();
        assert_eq!(head.operation_id, operation);
        assert_eq!(head.status, "LexicalOnly");
    }
}
#[test]
fn cancellation_while_waiting_for_sql_writer_cannot_publish() {
    let f = Fixture::new();
    let (owner, op) = f.source("writer-cancel", "# Heading\nalpha");
    let store = f.store.clone();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        write(&*store, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    entered
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let index = f.index.clone();
    let cancellation = Cancellation::new();
    let cancel = cancellation.clone();
    let (started_tx, started) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        index.run_index(&op, &cancellation, None)
    });
    started
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    cancel.cancel();
    release.send(()).unwrap();
    blocker.join().unwrap();
    assert!(matches!(
        worker.join().unwrap(),
        Err(StorageError::Cancelled)
    ));
    assert!(
        read(&*f.store, |s| s
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
}
fn completion_failure(f: &Fixture, enabled: bool) {
    let connection = rusqlite::Connection::open(f.h.root.join("workspace.sqlite")).unwrap();
    if enabled {
        connection.execute_batch("CREATE TRIGGER fail_index_completion BEFORE UPDATE OF observations_json ON operation_attempts WHEN EXISTS(SELECT 1 FROM json_each(NEW.observations_json) WHERE json_type(value,'$.conventionPublication') IS NOT NULL) BEGIN SELECT RAISE(ABORT,'controlled index completion failure');END;").unwrap();
    } else {
        connection
            .execute_batch("DROP TRIGGER fail_index_completion;")
            .unwrap();
    }
}
#[test]
fn one_head_and_completion_are_one_atomic_transaction() {
    let f = Fixture::new();
    let (owner, op) = f.source("atomic-one", "# Policy\nalpha");
    completion_failure(&f, true);
    assert!(f.index.run_index(&op, &Cancellation::new(), None).is_err());
    completion_failure(&f, false);
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .is_none(),
        "head must roll back if immutable completion cannot be recorded"
    );
    assert!(matches!(
        f.index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
    let actual = read(&*f.store, |r| r.operations().operation(&op.id)).unwrap();
    assert_eq!(
        actual
            .attempts
            .iter()
            .flat_map(|a| &a.observations)
            .filter(|v| v.get("conventionPublication").is_some())
            .count(),
        1
    );
}
#[test]
fn captured_all_applied_facts_survive_lost_ack_and_changed_catalogue() {
    let f = Fixture::new();
    for n in 0..3 {
        f.source(&format!("original-{n}"), "alpha");
    }
    let revision = read(&*f.store, |r| {
        r.conventions()
            .convention_scope_revision(&KnowledgeScope::Workspace)
    });
    let gateway = devworks_core::gateway::Gateway {
        authority: &Authority,
        store: &*f.store,
        ids: f.h.ids.as_ref(),
    };
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"all-crash","schemaRevision":3,"body":{"kind":"conventions.index","allScope":{"kind":"Workspace"},"expectedRevision":revision.to_string()}});
    let devworks_core::protocol::CommandReply::Recorded { receipt, .. } = gateway
        .command(
            &f.ctx,
            "conventions.index",
            frame.to_string().as_bytes(),
            Arc::new(IndexCommand(f.index.clone())),
        )
        .unwrap()
    else {
        panic!("durable originalAll")
    };
    let id = receipt.operation_id.unwrap();
    let op = dispatch(&*f.store, &id);
    completion_failure(&f, true);
    assert!(f.index.run_index(&op, &Cancellation::new(), None).is_err());
    completion_failure(&f, false);
    let actual = read(&*f.store, |r| r.operations().operation(&id)).unwrap();
    assert!(
        actual
            .attempts
            .iter()
            .flat_map(|a| &a.observations)
            .any(|v| v["conventionCapture"]["count"] == 3)
    );
    let headers = read(&*f.store, |r| {
        r.conventions()
            .convention_index_generations(None, Some(&id), None, 32)
    });
    assert_eq!(headers.len(), 3);
    assert!(headers.iter().all(|h| h.state == "Published"));
    let (new, _) = f.source("new-after-completion", "beta");
    let unknown = write(&*f.store, move |tx| {
        let current = tx.operations().operation(&id)?.unwrap();
        tx.operations().transition(
            &id,
            current.revision,
            current.latest_attempt().map(|a| a.fence()).as_ref(),
            devworks_core::protocol::OperationState::OutcomeUnknown,
            None,
        )
    })
    .unwrap();
    let IndexOutcome::Completed(value) = f
        .index
        .run_index(&unknown, &Cancellation::new(), None)
        .unwrap()
    else {
        panic!("observe completed originaltargetset")
    };
    assert_eq!(value["indexed"], 3);
    assert_eq!(value["conventions"], 3);
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&new.id))
        .is_none(),
        "recovery must not silentlyexpand the captured targetset"
    );
}
#[test]
fn historical_body_never_borrows_new_body_index_but_unchanged_body_revisions_share_it() {
    let f = Fixture::new();
    let (old, op) = f.source("history", "old body");
    f.index.run_index(&op, &Cancellation::new(), None).unwrap();
    let staged = f
        .artifacts
        .stage_host_bytes(
            &*f.store,
            b"new authored body",
            json!({"fixture":"new body"}),
        )
        .unwrap()
        .unwrap();
    let (service, pending, index, ctx, mut next) = (
        f.artifacts.clone(),
        staged.clone(),
        f.index.clone(),
        f.ctx.clone(),
        old.clone(),
    );
    let (next, operation) = write(&*f.store, move |tx| {
        let version = service.commit_host_bytes(
            tx,
            &pending,
            "new authored body",
            "text/markdown",
            json!({"fixture":"new body"}),
        )?;
        next.revision = 2;
        next.content_ref = Some(json!(version_ref(&version)));
        tx.conventions().revise_convention(&next, 1)?;
        let operation = index.record_changed(tx, &ctx, &next)?;
        Ok((next, operation))
    })
    .unwrap();
    f.artifacts.settle_host_bytes(&*f.store, &staged, true);
    let before = read(&*f.store, |r| f.index.status(r, &old));
    assert_eq!(json!(before.status), "NotIndexed");
    assert_eq!(
        before.reason.unwrap().0,
        "HistoricalSnapshotNotCurrentIndex"
    );
    assert!(before.indexed_chunks.is_none());
    let op = dispatch(&*f.store, &operation);
    f.index.run_index(&op, &Cancellation::new(), None).unwrap();
    let historical = read(&*f.store, |r| f.index.status(r, &old));
    assert_eq!(json!(historical.status), "NotIndexed");
    assert!(historical.generation_id.is_none());
    let mut metadata = next;
    metadata.revision = 3;
    metadata.applicability = vec!["same-body".into()];
    let row = metadata.clone();
    write(&*f.store, move |tx| {
        tx.conventions().revise_convention(&row, 2)
    })
    .unwrap();
    let current = read(&*f.store, |r| f.index.status(r, &metadata));
    assert_eq!(json!(current.status), "LexicalOnly");
    assert_eq!(current.source_revision.unwrap().get(), 2);
    assert!(current.indexed_chunks.unwrap().get() > 0);
}
