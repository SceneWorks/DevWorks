//! Metadata compatibility over actual native recorded vectors and immutable paid custody.
use super::convention_search::{VECTOR, push, qualify, queued_source, source};
use super::*;
use devworks_core::{
    authority::HostAuthority,
    gateway::Gateway,
    protocol::{CommandReply, OperationId, OperationState, ProjectId, results::DomainDecision},
    services::conventions::{
        ConventionService,
        indexing::{
            IndexService,
            embedding::{
                ConventionEmbeddings, EmbeddedBatch, EmbeddingConfiguration, EmbeddingFailure,
                EmbeddingRequest,
            },
            publisher::IndexOutcome,
        },
    },
};
use devworks_engine::conventions::embedding::NativeEmbeddings;

fn command(
    f: &Fx,
    service: Arc<ConventionService>,
    ctx: &InvocationContext,
    key: &str,
    body: Value,
) {
    let operation = body["kind"].as_str().unwrap().to_owned();
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":key,"body":body});
    let authority = HostAuthority {
        store: &*f.store,
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*f.store,
        authority: &authority,
        ids: &*f.h.ids,
    };
    let reply = UseCases::new()
        .with_conventions(service)
        .command(&gateway, ctx, &operation, frame.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(reply, CommandReply::Recorded { ref receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{reply:?}"
    );
}
fn owner(f: &Fx, id: &Id) -> StoredConvention {
    read(&*f.store, |r| {
        r.conventions()
            .convention(id, None, &[KnowledgeScope::Workspace])
    })
    .unwrap()
}
fn sent(f: &Fx) -> usize {
    f.http.lock().unwrap().requests.lock().unwrap().len()
}
fn index_preparations(f: &Fx) -> Vec<OperationRecord> {
    read(&*f.store, |r| {
        r.operations().operations_for_capability(
            "conventions.embedding_prepare",
            &[
                OperationState::Dispatching,
                OperationState::OutcomeUnknown,
                OperationState::ObservedComplete,
            ],
            None,
            500,
        )
    })
    .into_iter()
    .filter(|o| o.payload["origin"]["kind"] == "Index")
    .collect()
}
fn metadata(f: &Fx, service: Arc<ConventionService>, ctx: &InvocationContext, id: &Id, key: &str) {
    let before = owner(f, id);
    let source_revision = read(&*f.store, |r| {
        r.conventions()
            .convention_index_source_revision(&before.scope)
    });
    let body = before.content_ref.clone().unwrap();
    command(
        f,
        service.clone(),
        ctx,
        &format!("{key}-edit"),
        json!({"kind":"conventions.edit","conventionId":id,"expectedRevision":before.revision.to_string(),"content":body,"slug":format!("renamed-{key}"),"sortOrder":"42","rawContent":{"kind":"Set","content":body},"resources":[{"kind":"Attach","name":"attachment metadata","content":body}],"scope":{"kind":"Workspace"}}),
    );
    let next = owner(f, id);
    assert_eq!(next.content_ref, before.content_ref);
    assert_eq!(next.title, before.title);
    command(
        f,
        service,
        ctx,
        &format!("{key}-applicability"),
        json!({"kind":"conventions.applicability","conventionId":id,"expectedRevision":next.revision.to_string(),"applicability":["Rust"],"lifecycle":"Active","scope":{"kind":"Workspace"}}),
    );
    assert_eq!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_source_revision(&before.scope)),
        source_revision
    );
}
fn searchable(
    f: &Fx,
    service: Arc<ConventionService>,
    ctx: &InvocationContext,
    id: &Id,
    key: &str,
) {
    push(f);
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Query","queryId":key,"body":{"kind":"conventions.search","text":"alpha","scope":{"kind":"Workspace"},"applicability":["Rust"],"page":{"limit":10}}});
    let authority = HostAuthority {
        store: &*f.store,
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*f.store,
        authority: &authority,
        ids: &*f.h.ids,
    };
    let result = json!(
        UseCases::new()
            .with_conventions(service)
            .query(
                &gateway,
                ctx,
                "conventions.search",
                frame.to_string().as_bytes()
            )
            .unwrap()
    );
    assert_eq!(
        result["result"]["value"]["retrieval"]["mode"], "Hybrid",
        "{result}"
    );
    assert_eq!(
        result["result"]["value"]["items"][0]["convention"]["id"],
        json!(id),
        "{result}"
    );
}
fn service(f: &Fx, native: Arc<NativeEmbeddings>) -> Arc<ConventionService> {
    let service = Arc::new(ConventionService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    service.indexing().set_embeddings(Some(native));
    service
}
fn all_index(
    f: &Fx,
    service: Arc<ConventionService>,
    ctx: &InvocationContext,
    key: &str,
) -> OperationRecord {
    let revision = read(&*f.store, |r| {
        r.conventions()
            .convention_scope_revision(&KnowledgeScope::Workspace)
    });
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":key,"body":{"kind":"conventions.index","allScope":{"kind":"Workspace"},"expectedRevision":revision.to_string()}});
    let authority = HostAuthority {
        store: &*f.store,
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*f.store,
        authority: &authority,
        ids: &*f.h.ids,
    };
    let reply = UseCases::new()
        .with_conventions(service)
        .command(
            &gateway,
            ctx,
            "conventions.index",
            frame.to_string().as_bytes(),
        )
        .unwrap();
    let CommandReply::Recorded { receipt, .. } = reply else {
        panic!("{reply:?}")
    };
    let DomainDecision::Accepted { operation_id } = receipt.decision else {
        panic!("{:?}", receipt.decision)
    };
    read(&*f.store, |r| r.operations().operation(&operation_id)).unwrap()
}
fn dispatch(f: &Fx, op: &OperationRecord) -> OperationRecord {
    let key = op.id.clone();
    write(&*f.store, move |tx| {
        let current = tx.operations().operation(&key)?.unwrap();
        let attempt =
            tx.operations()
                .begin_attempt(&key, current.revision, AttemptBinding::default())?;
        let current = tx.operations().operation(&key)?.unwrap();
        tx.operations().transition(
            &key,
            current.revision,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )
    })
    .unwrap()
}
fn unknown(f: &Fx, op: &OperationRecord) -> OperationRecord {
    let key = op.id.clone();
    write(&*f.store, move |tx| {
        let current = tx.operations().operation(&key)?.unwrap();
        tx.operations().transition(
            &key,
            current.revision,
            current.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::OutcomeUnknown,
            None,
        )
    })
    .unwrap()
}

#[test]
fn queued_initial_index_survives_real_metadata_raw_attachment_and_active_applicability_edits() {
    queued_metadata(false);
}
#[test]
fn queued_all_preserves_actual_source_membership_across_metadata_before_capture() {
    queued_metadata(true);
}
fn queued_metadata(all: bool) {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, _) = qualify(&f);
    let service = service(&f, native);
    let index = service.indexing();
    let (row, mut op) = queued_source(&f, index.clone(), &ctx, "queued-compatible");
    if all {
        command(
            &f,
            service.clone(),
            &ctx,
            "cancel-redundant-one",
            json!({"kind":"conventions.retire","operationId":op.id,"expectedRevision":op.revision.to_string(),"scope":{"kind":"Workspace"}}),
        );
        op = all_index(&f, service.clone(), &ctx, "queued-all");
        assert!(op.payload["indexSourceRevision"].is_i64());
    }
    assert_eq!(op.state, OperationState::IntentRecorded);
    let original = read(&*f.store, |r| {
        r.conventions().convention_index_status(&row.id)
    });
    if all {
        assert!(
            original.is_none(),
            "cancelled redundant One is not active status"
        );
    } else {
        assert_eq!(original.as_ref().unwrap().status, "Queued");
    }
    metadata(&f, service.clone(), &ctx, &row.id, "queued");
    assert_eq!(owner(&f, &row.id).revision, 3);
    push(&f);
    let dispatcher = dispatcher_with(
        &f.h,
        f.store.clone(),
        vec![devworks_engine::conventions::indexing::consumer(
            f.store.clone(),
            index.clone(),
        )],
        |_| {},
    );
    runtime().block_on(async {
        dispatcher.recover().await.unwrap();
        dispatcher.run_until_idle().await.unwrap();
    });
    assert_eq!(
        read(&*f.store, |r| r.operations().operation(&op.id))
            .unwrap()
            .state,
        OperationState::ObservedComplete
    );
    let head = read(&*f.store, |r| {
        r.conventions().convention_index_head(&row.id)
    })
    .unwrap();
    if !all {
        assert_eq!(head.generation_id, original.unwrap().generation_id);
    }
    assert_eq!(
        head.convention_revision,
        if all { 3 } else { 1 },
        "captured source and native identities stay immutable"
    );
    assert_eq!(head.status, "Ready");
    assert_eq!(sent(&f), 2, "one probe plus the original index request");
    searchable(&f, service, &ctx, &row.id, "compatible-query");
}

struct GateAck {
    native: Arc<NativeEmbeddings>,
    ready: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl ConventionEmbeddings for GateAck {
    fn is_available(&self) -> bool {
        self.native.is_available()
    }
    fn configured_space(
        &self,
        ctx: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, EmbeddingFailure> {
        self.native.configured_space(ctx)
    }
    fn embed(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        let request_id = request.request_id.clone();
        self.native.embed(ctx, request)?;
        self.ready.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        Err(EmbeddingFailure::Unknown {
            request_id,
            reason: "controlled lost acknowledgement after actual native close".into(),
        })
    }
    fn observe(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        self.native.observe(ctx, request)
    }
}
#[test]
fn running_index_metadata_edit_keeps_original_paid_unknown_request_and_observes_without_resend() {
    running_metadata(false);
}
#[test]
fn running_all_metadata_preserves_original_header_and_paid_unknown_preparation() {
    running_metadata(true);
}
fn running_metadata(all: bool) {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, _) = qualify(&f);
    let service = service(&f, native.clone());
    let index = service.indexing();
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    let (release, receiver) = std::sync::mpsc::sync_channel(1);
    index.set_embeddings(Some(Arc::new(GateAck {
        native: native.clone(),
        ready,
        release: std::sync::Mutex::new(receiver),
    })));
    let (row, one) = source(&f, index.clone(), &ctx, "running-compatible");
    let op = if all {
        f.h.advance(1);
        dispatch(&f, &all_index(&f, service.clone(), &ctx, "running-all"))
    } else {
        one
    };
    push(&f);
    std::thread::scope(|threads| {
        let worker = threads.spawn(|| index.run_index(&op, &Cancellation::new(), None));
        waiting
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        assert_eq!(
            read(&*f.store, |r| r
                .conventions()
                .convention_index_status(&row.id))
            .unwrap()
            .status,
            "Indexing"
        );
        metadata(&f, service.clone(), &ctx, &row.id, "running");
        release.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap().unwrap(),
            IndexOutcome::Unknown(_)
        ));
    });
    let preparation = index_preparations(&f).pop().unwrap();
    assert_eq!(sent(&f), 2);
    let unknown = unknown(&f, &op);
    assert!(matches!(
        index
            .run_index(&unknown, &Cancellation::new(), None)
            .unwrap(),
        IndexOutcome::Completed(_)
    ));
    let after = index_preparations(&f);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, preparation.id);
    assert_eq!(after[0].args_digest, preparation.args_digest);
    assert_eq!(
        after[0].payload["requestId"],
        preparation.payload["requestId"]
    );
    assert_eq!(
        sent(&f),
        2,
        "exact original closed native request is observed, not repeated"
    );
    index.set_embeddings(Some(native));
    searchable(&f, service, &ctx, &row.id, "recovered-compatible-query");
}

#[path = "convention_index_compatibility/partial.rs"]
mod partial;

#[test]
fn body_title_and_retirement_changes_still_fence_original_pending_source_before_http() {
    for changed in ["body", "same-bytes-new-version", "title", "retirement"] {
        let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
        let (ctx, native, _) = qualify(&f);
        let service = service(&f, native);
        let index = service.indexing();
        let (row, op) = source(&f, index.clone(), &ctx, changed);
        let before_source = read(&*f.store, |r| {
            r.conventions()
                .convention_index_source_revision(&KnowledgeScope::Workspace)
        });
        let mut body = json!({"kind":"conventions.edit","conventionId":row.id,"expectedRevision":"1","content":row.content_ref,"scope":{"kind":"Workspace"}});
        if changed == "retirement" {
            body = json!({"kind":"conventions.applicability","conventionId":row.id,"expectedRevision":"1","applicability":[],"lifecycle":"Retired","scope":{"kind":"Workspace"}});
        } else if changed == "title" {
            body["title"] = json!("A different indexed title");
        } else {
            let staged = f
                .artifacts
                .stage_host_bytes(
                    &*f.store,
                    if changed == "same-bytes-new-version" {
                        b"# Policy\nalpha exact authored rule"
                    } else {
                        b"# Different\nchanged body"
                    },
                    json!({"fixture":"new authored body"}),
                )
                .unwrap()
                .unwrap();
            let (artifacts, pending) = (f.artifacts.clone(), staged.clone());
            let version = write(&*f.store, move |tx| {
                artifacts.commit_host_bytes(
                    tx,
                    &pending,
                    "new body",
                    "text/markdown",
                    json!({"fixture":"new authored body"}),
                )
            })
            .unwrap();
            f.artifacts.settle_host_bytes(&*f.store, &staged, true);
            body["content"] = json!(version_ref(&version));
        }
        command(&f, service, &ctx, &format!("change-{changed}"), body);
        assert_eq!(
            read(&*f.store, |r| r
                .conventions()
                .convention_index_source_revision(&KnowledgeScope::Workspace)),
            before_source + 1
        );
        assert!(
            matches!(
                index.run_index(&op, &Cancellation::new(), None),
                Err(StorageError::InvalidRecord(_))
            ),
            "{changed} must fence the captured source"
        );
        assert!(
            read(&*f.store, |r| r
                .conventions()
                .convention_index_head(&row.id))
            .is_none()
        );
        assert_eq!(
            sent(&f),
            1,
            "changed indexed source cannot start the old paid request"
        );
    }
}
