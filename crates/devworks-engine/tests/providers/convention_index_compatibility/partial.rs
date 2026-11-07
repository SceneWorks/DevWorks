use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
struct LoseCapture {
    store: Arc<dyn ApplicationStore>,
    operation: OperationId,
    armed: Arc<AtomicBool>,
}
struct RecordedResponses {
    native: Arc<NativeEmbeddings>,
    http: Arc<ScriptHttp>,
}
impl ConventionEmbeddings for RecordedResponses {
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
        // One recorded JSON response per actual HTTP request; body chunks within a stream
        // are not a queue of separate responses.
        let mut script = self.http.lock().unwrap();
        assert!(script.body.is_empty());
        script.body.push_back(VECTOR.as_bytes().to_vec());
        drop(script);
        self.native.embed(c, r)
    }
    fn observe(
        &self,
        c: &InvocationContext,
        r: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        self.native.observe(c, r)
    }
}
struct CaptureJob {
    job: Box<dyn WriteJob>,
    operation: OperationId,
    armed: Arc<AtomicBool>,
    hide: bool,
}
impl WriteJob for CaptureJob {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.job.run(tx)?;
        if !tx
            .conventions()
            .convention_index_generations(None, Some(&self.operation), None, 1)?
            .is_empty()
            && self.armed.swap(false, Ordering::SeqCst)
        {
            self.hide = true;
        }
        Ok(())
    }
    fn finish(self: Box<Self>, result: Result<(), StorageError>) {
        self.job.finish(if self.hide && result.is_ok() {
            Err(StorageError::CommitUnknown)
        } else {
            result
        });
    }
}
impl ApplicationStore for LoseCapture {
    fn info(&self) -> StoreInfo {
        self.store.info()
    }
    fn read(
        &self,
        o: &ReadOptions,
        f: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.store.read(o, f)
    }
    fn submit_write(&self, o: WriteOptions, job: Box<dyn WriteJob>) -> Result<(), StorageError> {
        self.store.submit_write(
            o,
            Box::new(CaptureJob {
                job,
                operation: self.operation.clone(),
                armed: self.armed.clone(),
                hide: false,
            }),
        )
    }
}
fn interrupted_capture(
    f: &Fx,
    native: Arc<NativeEmbeddings>,
    op: &OperationRecord,
) -> Arc<IndexService> {
    let index = Arc::new(IndexService::new(
        Arc::new(LoseCapture {
            store: f.store.clone(),
            operation: op.id.clone(),
            armed: Arc::new(AtomicBool::new(true)),
        }),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    ));
    index.set_embeddings(Some(Arc::new(RecordedResponses {
        native,
        http: f.http.clone(),
    })));
    assert!(matches!(
        index.run_index(op, &Cancellation::new(), None),
        Err(StorageError::CommitUnknown)
    ));
    assert_eq!(sent(f), 1, "partial capture precedes any native request");
    assert!(index_preparations(f).is_empty());
    index
}
fn headers(f: &Fx, op: &OperationRecord) -> Vec<StoredConventionIndex> {
    read(&*f.store, |r| {
        r.conventions()
            .convention_index_generations(None, Some(&op.id), None, 500)
    })
}
#[test]
fn all_partial_capture_metadata_resume_keeps_original_32_headers_and_publishes_all_33_once() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, _) = qualify(&f);
    let service = service(&f, native.clone());
    for n in 0..33 {
        queued_source(&f, service.indexing(), &ctx, &format!("page-{n:02}"));
    }
    f.h.advance(1);
    let op = dispatch(&f, &all_index(&f, service.clone(), &ctx, "partial-all"));
    let index = interrupted_capture(&f, native, &op);
    let original = headers(&f, &op);
    assert_eq!(original.len(), 32);
    let mut duplicate = original[0].clone();
    duplicate.generation_id = id("mismatched-extra-generation");
    assert!(
        write(&*f.store, move |tx| tx
            .conventions()
            .stage_convention_index(&duplicate))
        .is_err(),
        "one operation cannot select two generations for the same owner"
    );
    assert_eq!(headers(&f, &op).len(), 32);
    let edited = original[0].convention_id.clone();
    metadata(
        &f,
        service.clone(),
        &ctx,
        &original[0].convention_id,
        "partial",
    );
    let unknown = unknown(&f, &op);
    let IndexOutcome::Completed(result) = index
        .run_index(&unknown, &Cancellation::new(), None)
        .unwrap()
    else {
        panic!("original All must finish");
    };
    assert_eq!(result["conventions"], 33);
    let final_headers = headers(&f, &op);
    assert_eq!(final_headers.len(), 33);
    for before in original {
        let after = final_headers
            .iter()
            .find(|g| g.convention_id == before.convention_id)
            .unwrap();
        assert_eq!(after.generation_id, before.generation_id);
        assert_eq!(after.convention_revision, before.convention_revision);
        assert_eq!(after.status, "Ready");
    }
    assert_eq!(index_preparations(&f).len(), 33);
    assert_eq!(sent(&f), 34);
    assert!(matches!(
        index
            .run_index(&unknown, &Cancellation::new(), None)
            .unwrap(),
        IndexOutcome::Completed(_)
    ));
    assert_eq!(sent(&f), 34, "completed facts never resend");
    searchable(&f, service, &ctx, &edited, "partial-ready-query");
}
#[test]
fn queued_and_partial_all_reject_actual_title_or_membership_changes_before_additional_paid_work() {
    for partial in [false, true] {
        for mutation in ["title", "member"] {
            let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
            let (ctx, native, _) = qualify(&f);
            let service = service(&f, native.clone());
            let (row, _) = queued_source(&f, service.indexing(), &ctx, "original");
            let op = dispatch(
                &f,
                &all_index(&f, service.clone(), &ctx, "reject-mutated-all"),
            );
            let index = if partial {
                interrupted_capture(&f, native, &op)
            } else {
                service.indexing()
            };
            let captured = headers(&f, &op);
            if mutation == "title" {
                command(
                    &f,
                    service.clone(),
                    &ctx,
                    "change-title",
                    json!({"kind":"conventions.edit","conventionId":row.id,"expectedRevision":"1","content":row.content_ref,"title":"Actual changed title","scope":{"kind":"Workspace"}}),
                );
            } else {
                queued_source(&f, service.indexing(), &ctx, "new-active-member");
            }
            let resumed = if partial {
                unknown(&f, &op)
            } else {
                op.clone()
            };
            assert!(
                matches!(
                    index.run_index(&resumed, &Cancellation::new(), None),
                    Err(StorageError::InvalidRecord(_))
                ),
                "{partial}/{mutation}"
            );
            let after = headers(&f, &op);
            assert_eq!(after.len(), captured.len());
            for (before, after) in captured.iter().zip(&after) {
                assert_eq!(before.generation_id, after.generation_id);
                assert_eq!(before.source_ref, after.source_ref);
                assert_eq!(before.convention_revision, after.convention_revision);
            }
            assert_eq!(sent(&f), 1);
            assert!(index_preparations(&f).is_empty());
            assert!(
                read(&*f.store, |r| r
                    .conventions()
                    .convention_index_head(&row.id))
                .is_none()
            );
        }
    }
}
#[test]
fn source_revision_move_bumps_both_scopes_and_checked_overflow_rolls_back_owner() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, _) = qualify(&f);
    let service = service(&f, native);
    let (row, _) = queued_source(&f, service.indexing(), &ctx, "moving-source");
    let project = ProjectId(id("destination"));
    let destination = project.clone();
    write(&*f.store, move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: destination,
            name: "Destination".into(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })
    })
    .unwrap();
    let before = read(&*f.store, |r| {
        r.conventions()
            .convention_index_source_revision(&KnowledgeScope::Workspace)
    });
    command(
        &f,
        service,
        &ctx,
        "move-source",
        json!({"kind":"conventions.move","conventionId":row.id,"expectedRevision":"1","destination":{"kind":"Project","projectId":project},"scope":{"kind":"Workspace"}}),
    );
    assert_eq!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_source_revision(&KnowledgeScope::Workspace)),
        before + 1
    );
    assert_eq!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_source_revision(&KnowledgeScope::Project(
                project.clone()
            ))),
        2
    );
    let current = read(&*f.store, |r| {
        r.conventions()
            .convention(&row.id, None, &[KnowledgeScope::Project(project.clone())])
    })
    .unwrap();
    let sql = rusqlite::Connection::open(f.h.root.join("workspace.sqlite")).unwrap();
    assert!(
        sql.execute(
            "UPDATE convention_scope_revisions SET index_source_revision=0 WHERE scope_key=?1",
            [KnowledgeScope::Project(project.clone()).key()]
        )
        .is_err()
    );
    sql.execute(
        "UPDATE convention_scope_revisions SET index_source_revision=?1 WHERE scope_key=?2",
        rusqlite::params![i64::MAX, KnowledgeScope::Project(project.clone()).key()],
    )
    .unwrap();
    let result = write(&*f.store, move |tx| {
        let expected = current.revision;
        let mut changed = current;
        changed.revision += 1;
        changed.title = "Overflow cannot publish".into();
        tx.conventions().revise_convention(&changed, expected)
    });
    assert!(matches!(result, Err(StorageError::InvalidRecord(_))));
    assert_eq!(
        read(&*f.store, |r| r.conventions().convention(
            &row.id,
            None,
            &[KnowledgeScope::Project(project)]
        ))
        .unwrap()
        .revision,
        2
    );
    assert_eq!(sent(&f), 1);
}

#[test]
fn retained_legacy_all_journal_without_semantic_witness_keeps_original_strict_scope_fence() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, native, _) = qualify(&f);
    let service = service(&f, native);
    let (row, _) = queued_source(&f, service.indexing(), &ctx, "legacy-source");
    let original = all_index(&f, service.clone(), &ctx, "original-all-shape");
    // Persist the exact old private journal shape through the real operation/queue ports,
    // including its original public scope CAS and actor/source/authority bindings.
    let legacy=write(&*f.store,move|tx|{
        let mut payload=original.payload;
        payload.as_object_mut().unwrap().remove("indexSourceRevision");
        let delivery=id("legacy-index-delivery");payload["deliveryId"]=json!(delivery);
        let key=OperationId(id("retained-legacy-index"));
        tx.operations().record_intent(NewOperation{id:key.clone(),root_id:original.root_id,assignment_id:original.assignment_id,capability:original.capability,schema_version:original.schema_version,args_digest:original.args_digest,payload})?;
        let event=common::event(tx,"RetainedLegacyIndexIntent")?;let now=tx.now_ms();
        tx.queue().enqueue(NewQueueEntry{id:delivery,consumer:devworks_core::services::conventions::indexing::commands::EFFECT_CONSUMER.into(),event_id:event,root_id:None,operation_id:Some(key.clone()),partition:None,priority:0,ready_at_ms:now,payload:json!({"kind":devworks_core::services::conventions::indexing::commands::JOB_KIND})})?;
        Ok(tx.operations().operation(&key)?.unwrap())
    }).unwrap();
    metadata(&f, service.clone(), &ctx, &row.id, "legacy-metadata");
    let current = dispatch(&f, &legacy);
    assert!(matches!(
        service
            .indexing()
            .run_index(&current, &Cancellation::new(), None),
        Err(StorageError::InvalidRecord(_))
    ));
    assert_eq!(sent(&f), 1);
    assert!(index_preparations(&f).is_empty());
}
