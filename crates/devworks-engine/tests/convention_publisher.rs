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
    assert!(matches!(
        f.index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
    let (head, count) = read(&*f.store, |s| {
        let head = s.conventions().convention_index_head(&owner.id)?.unwrap();
        let count = s
            .conventions()
            .convention_chunk_count(&head.generation_id)?;
        Ok((head, count))
    });
    assert_eq!(head.status, "LexicalOnly");
    assert!(count > 4);
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
