//! Actual gateway admission, SQLite private staging, managed file bytes and durable consumer.
//! No inference, remote transport, fabricated artifact refs or synthetic parser results.
mod common;
use common::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::{
    authority::HostAuthority,
    context::{InvocationContext, PrincipalKind},
    gateway::{AdmittedCommand, Gateway},
    operations::{OperationInput, conventions as ops},
    ports::{CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared, storage::*},
    protocol::{
        CommandReply, CorrelationId, OperationId, OperationState, provider::ArtifactRef,
        results::DomainDecision,
    },
    services::{
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        conventions::{importing::ImportService, indexing::IndexService, model},
        sessions::Failure,
    },
};
use devworks_engine::dispatch;
use serde_json::json;
use std::sync::Arc;
// Compile the actual production consumer while composition is integrated by its runtime owner.
#[path = "../src/conventions_import.rs"]
mod actual_consumer;
struct Handler(Arc<ImportService>);
fn input(c: &AdmittedCommand) -> &ops::ImportInput {
    let OperationInput::Conventions(ops::Input::Import(i)) = c.input() else {
        panic!("import frame")
    };
    i
}
impl CommandHandler for Handler {
    fn prepare(
        &self,
        c: &InvocationContext,
        a: &AdmittedCommand,
        e: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        self.0
            .prepare_import(c, input(a), e)
            .map(|p| Some(Prepared::new(p)))
            .map_err(|e| match e {
                Failure::Storage(e) => e,
                _ => StorageError::ScopeDenied,
            })
    }
    fn decide(
        &self,
        c: &InvocationContext,
        a: &AdmittedCommand,
        t: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        match self.0.decide_import(c, a, t, input(a)) {
            Ok(d) => Ok(d),
            Err(Failure::Storage(e)) => Err(e.into()),
            Err(Failure::Refused(e)) => Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: e.fault(CorrelationId(t.new_id(IdKind::Correlation))),
                },
                operation_id: None,
            }),
        }
    }
}
struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    service: Arc<ImportService>,
    ctx: InvocationContext,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        let ctx = principal(store.as_ref(), "actual-human", PrincipalKind::Human);
        let artifacts = Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("import-cache"),
            )),
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        ));
        let index = Arc::new(IndexService::new(
            store.clone(),
            artifacts.clone(),
            h.ids.clone(),
            h.clock(),
        ));
        let service = Arc::new(
            ImportService::new(store.clone(), artifacts.clone(), h.ids.clone(), h.clock())
                .with_index(index),
        );
        Self {
            h,
            store,
            artifacts,
            service,
            ctx,
        }
    }
    fn source(&self, bytes: &[u8]) -> ArtifactRef {
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                bytes,
                json!({"kind":"NativeImportFixtureUpload"}),
            )
            .unwrap()
            .unwrap();
        let (a, s, actor) = (
            self.artifacts.clone(),
            staged.clone(),
            self.ctx.principal().clone(),
        );
        let version = write(self.store.as_ref(), move |tx| {
            a.commit_host_bytes_in_scope(
                tx,
                &s,
                HostArtifactPublication {
                    title: "Uploaded portable source",
                    media_type: "application/json",
                    scope: ArtifactScope::Workspace,
                    owner: actor,
                    provenance: json!({"kind":"NativeImportFixtureUpload"}),
                },
            )
        })
        .unwrap();
        self.artifacts
            .settle_host_bytes(self.store.as_ref(), &staged, true);
        version_ref(&version)
    }
    fn admit(&self, reference: &ArtifactRef, key: &str) -> OperationId {
        let authority = HostAuthority {
            store: self.store.as_ref(),
            clock: &*self.h.clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            store: self.store.as_ref(),
            authority: &authority,
            ids: &*self.h.ids,
        };
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":key,"body":{"kind":"conventions.import","scope":{"kind":"Workspace"},"source":reference,"schema":model::manifest_schema()}});
        match gateway
            .command(
                &self.ctx,
                "conventions.import",
                frame.to_string().as_bytes(),
                Arc::new(Handler(self.service.clone())),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Accepted { operation_id } => operation_id,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
    fn dispatch(&self, op: &OperationId) -> OperationRecord {
        let dispatcher = dispatcher_with(
            &self.h,
            self.store.clone(),
            vec![actual_consumer::consumer(self.service.clone())],
            |_| {},
        );
        runtime().block_on(async {
            dispatcher.recover().await.unwrap();
            dispatcher.run_until_idle().await.unwrap();
        });
        read(self.store.as_ref(), |r| r.operations().operation(op)).unwrap()
    }
}
fn manifest(resources: usize) -> serde_json::Value {
    let data = b"\0\x01\xffresource";
    json!({"format":"devworks-convention","version":1,"slug":"exact\0slug","title":"Exact title\0","parentPath":[],"sortOrder":-2147483648_i64,"lifecycle":"Active","applicability":["Rust"],"markdown":{"sha256":devworks_core::fingerprint::sha256_hex(b"# Actual\n\0body"),"byteLength":"14","text":"# Actual\n\0body"},"rawContent":{"sha256":devworks_core::fingerprint::sha256_hex(b""),"byteLength":"0","text":""},"resources":(0..resources).map(|i|json!({"key":format!("foreign-{i}"),"name":format!("resource-{i}"),"mediaType":"","lifecycle":"Retired","sha256":devworks_core::fingerprint::sha256_hex(data),"byteLength":data.len().to_string(),"base64":"AAH/cmVzb3VyY2U="})).collect::<Vec<_>>()})
}
#[test]
fn durable_import_publishes_verified_exact_sources_and_fresh_resources_atomically() {
    let f = Fx::new();
    let bytes = serde_json::to_vec(&manifest(3)).unwrap();
    let source = f.source(&bytes);
    let op = f.admit(&source, "actual-import");
    let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    assert_eq!(draft.phase, "Queued");
    assert!(
        read(f.store.as_ref(), |r| r.conventions().convention(
            &draft.convention_id,
            None,
            &[KnowledgeScope::Workspace]
        ))
        .is_none()
    );
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::ObservedComplete, "{record:?}");
    let owner = read(f.store.as_ref(), |r| {
        r.conventions()
            .convention(&draft.convention_id, None, &[KnowledgeScope::Workspace])
    })
    .unwrap();
    assert_eq!(owner.slug, "exact\0slug");
    assert_eq!(owner.author_id, *f.ctx.principal());
    assert_eq!(owner.provenance["actorId"], json!(f.ctx.principal()));
    let content: ArtifactRef = serde_json::from_value(owner.content_ref.unwrap()).unwrap();
    assert_eq!(
        f.artifacts
            .read_exact(f.store.as_ref(), &f.ctx, &content, 100)
            .unwrap()
            .0,
        b"# Actual\n\0body"
    );
    let resources = read(f.store.as_ref(), |r| {
        r.conventions().convention_resources(
            &ConventionResourceQuery {
                scopes: vec![KnowledgeScope::Workspace],
                convention_id: Some(draft.convention_id.clone()),
                convention_revision: Some(1),
                lifecycles: vec!["Retired".into()],
            },
            None,
            10,
        )
    });
    assert_eq!(resources.len(), 3);
    assert!(resources.iter().all(|r| r.revision == 1
        && r.media_type.is_empty()
        && !r.id.as_str().starts_with("foreign")));
    let again = f.service.observe(&record);
    assert!(
        matches!(
            again,
            devworks_core::services::conventions::importing::ImportOutcome::Completed(_)
        ),
        "{again:?}"
    );
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &f.ctx, &source, bytes.len() as u64)
            .is_ok()
    );
}
#[test]
fn bad_manifest_rolls_back_publication_and_cancel_never_deletes_input() {
    let f = Fx::new();
    let mut m = manifest(2);
    m["unknown"] = json!(true);
    let source = f.source(&serde_json::to_vec(&m).unwrap());
    let op = f.admit(&source, "invalid-tail");
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::Failed, "{record:?}");
    let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    assert!(
        read(f.store.as_ref(), |r| r.conventions().convention(
            &draft.convention_id,
            None,
            &[KnowledgeScope::Workspace]
        ))
        .is_none()
    );
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &f.ctx, &source, 65536)
            .is_ok()
    );
    let captures = read(f.store.as_ref(), |r| {
        r.conventions().convention_import_artifacts(&op, None, 32)
    });
    assert!(
        !captures.is_empty(),
        "invalid tail followed real private staging"
    );
    for capture in &captures {
        assert!(
            read(f.store.as_ref(), |r| r
                .artifacts()
                .artifact(&capture.artifact.id))
            .is_none()
        );
        assert!(
            read(f.store.as_ref(), |r| r
                .artifacts()
                .version(&capture.version.id))
            .is_none()
        );
    }
    f.service.recover_pending().unwrap();
    assert!(
        read(f.store.as_ref(), |r| r
            .conventions()
            .convention_import_artifacts(&op, None, 32))
        .is_empty()
    );
    let source = f.source(&serde_json::to_vec(&manifest(0)).unwrap());
    let op = f.admit(&source, "cancel-before-dispatch");
    let saved = read(f.store.as_ref(), |r| r.operations().operation(&op)).unwrap();
    let (service, ctx, id) = (f.service.clone(), f.ctx.clone(), op.clone());
    write(f.store.as_ref(), move |tx| {
        service
            .cancel(
                tx,
                &ctx,
                &id,
                &devworks_core::protocol::Revision::new(saved.revision).unwrap(),
                None,
            )
            .map_err(|_| StorageError::ScopeDenied)
    })
    .unwrap();
    f.service.recover_pending().unwrap();
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::CancelledBeforeDispatch);
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &f.ctx, &source, 65536)
            .is_ok()
    );
}

#[test]
fn imports_more_than_one_resource_page_without_silent_cap_or_public_bundle() {
    let f = Fx::new();
    let source = f.source(&serde_json::to_vec(&manifest(501)).unwrap());
    let op = f.admit(&source, "wide-import");
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::ObservedComplete, "{record:?}");
    let d = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    assert_eq!(d.processed_resources, 501);
    assert_eq!(d.total_resources, Some(501));
    let query = ConventionResourceQuery {
        scopes: vec![KnowledgeScope::Workspace],
        convention_id: Some(d.convention_id.clone()),
        convention_revision: Some(1),
        lifecycles: vec!["Retired".into()],
    };
    let first = read(f.store.as_ref(), |r| {
        r.conventions().convention_resources(&query, None, 500)
    });
    assert_eq!(first.len(), 500);
    let last = read(f.store.as_ref(), |r| {
        r.conventions()
            .convention_resources(&query, first.last().map(|r| &r.id), 500)
    });
    assert_eq!(last.len(), 1);
    assert_ne!(first[0].id, last[0].id);
    assert_eq!(
        read(f.store.as_ref(), |r| r.queue().status(
            devworks_core::services::conventions::indexing::commands::EFFECT_CONSUMER
        ))
        .ready,
        1,
        "actual changed-body indexing queued; resources have no embedding intents"
    );
}

/// Lose a real writer acknowledgment after a selected private/public phase committed. All
/// repositories, transactions and bytes remain the actual native owners.
struct LostAckStore {
    inner: Arc<dyn ApplicationStore>,
    target: OperationId,
    phase: &'static str,
    armed: Arc<std::sync::atomic::AtomicBool>,
}
struct LostAckJob {
    inner: Box<dyn WriteJob>,
    target: OperationId,
    phase: &'static str,
    armed: Arc<std::sync::atomic::AtomicBool>,
    hide: bool,
}
impl WriteJob for LostAckJob {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.inner.run(tx)?;
        if tx
            .conventions()
            .convention_import(&self.target)?
            .is_some_and(|d| d.phase == self.phase)
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.hide = true;
        }
        Ok(())
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        self.inner.finish(if self.hide && outcome.is_ok() {
            Err(StorageError::CommitUnknown)
        } else {
            outcome
        });
    }
}
impl ApplicationStore for LostAckStore {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.inner.submit_write(
            options,
            Box::new(LostAckJob {
                inner: job,
                target: self.target.clone(),
                phase: self.phase,
                armed: self.armed.clone(),
                hide: false,
            }),
        )
    }
}
#[test]
fn actual_reopen_resumes_private_checkpoint_and_observes_publication_without_duplicates() {
    for phase in ["Parsing", "Published"] {
        let mut f = Fx::new();
        let source = f.source(&serde_json::to_vec(&manifest(3)).unwrap());
        let op = f.admit(&source, &format!("lose-{phase}"));
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        f.store = Arc::new(LostAckStore {
            inner: f.store.clone(),
            target: op.clone(),
            phase,
            armed: armed.clone(),
        });
        let index = Arc::new(IndexService::new(
            f.store.clone(),
            f.artifacts.clone(),
            f.h.ids.clone(),
            f.h.clock(),
        ));
        f.service = Arc::new(
            ImportService::new(
                f.store.clone(),
                f.artifacts.clone(),
                f.h.ids.clone(),
                f.h.clock(),
            )
            .with_index(index),
        );
        let record = f.dispatch(&op);
        assert_eq!(
            record.state,
            OperationState::OutcomeUnknown,
            "{phase}: {record:?}"
        );
        assert!(!armed.load(std::sync::atomic::Ordering::SeqCst));
        let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
        assert_eq!(draft.phase, phase);
        if phase == "Parsing" {
            assert!(draft.checkpoint["offset"].as_u64().unwrap() > 0);
            assert_eq!(
                read(f.store.as_ref(), |r| r.artifacts().list_workspace(
                    None,
                    &[],
                    None,
                    50
                ))
                .len(),
                1,
                "private committed prefix remains invisible"
            );
        }
        let Fx {
            h,
            store,
            artifacts,
            service,
            ctx,
        } = f;
        let prior_boot = store.info().boot_id;
        drop(service);
        drop(store);
        let store = h.open();
        assert_ne!(
            store.info().boot_id,
            prior_boot,
            "actual native reopen creates new boot"
        );
        let index = Arc::new(IndexService::new(
            store.clone(),
            artifacts.clone(),
            h.ids.clone(),
            h.clock(),
        ));
        let service = Arc::new(
            ImportService::new(store.clone(), artifacts.clone(), h.ids.clone(), h.clock())
                .with_index(index),
        );
        service.recover_pending().unwrap();
        let dispatcher = dispatcher_with(
            &h,
            store.clone(),
            vec![actual_consumer::consumer(service)],
            |_| {},
        );
        runtime().block_on(async {
            dispatcher.recover().await.unwrap();
            dispatcher.observe_unknown().await.unwrap();
            dispatcher.run_until_idle().await.unwrap();
        });
        let record = read(store.as_ref(), |r| r.operations().operation(&op)).unwrap();
        assert_eq!(
            record.state,
            OperationState::ObservedComplete,
            "{phase}: {record:?}"
        );
        let finished = read(store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
        assert_eq!(finished.phase, "Published");
        assert_eq!(finished.processed_resources, 3);
        assert_eq!(
            read(store.as_ref(), |r| r.artifacts().list_workspace(
                None,
                &[],
                None,
                50
            ))
            .len(),
            6,
            "source + two exact documents + three fresh resources once"
        );
        assert!(
            artifacts
                .read_exact(store.as_ref(), &ctx, &source, 65536)
                .is_ok()
        );
    }
}

#[test]
fn current_scope_and_quota_are_rechecked_after_admission_without_publishing_a_prefix() {
    for quota in [false, true] {
        let f = Fx::new();
        let source = f.source(&serde_json::to_vec(&manifest(3)).unwrap());
        let op = f.admit(&source, if quota { "quota-race" } else { "scope-race" });
        if quota {
            let limit = source.byte_length.get() + 13;
            write(f.store.as_ref(), move |tx| {
                let key = devworks_core::settings::catalog::ARTIFACT_QUOTA_KEY;
                let schema = devworks_core::settings::catalog::descriptor(key)
                    .unwrap()
                    .schema_ref();
                tx.configuration().append_revision(
                    &ConfigScope::Workspace,
                    "settings",
                    1,
                    1,
                    json!({"values":{key:{"schema":schema,"value":limit}}}),
                )?;
                Ok(())
            })
            .unwrap();
        } else {
            write(f.store.as_ref(), |tx| {
                tx.conventions()
                    .bump_convention_scope(&KnowledgeScope::Workspace)
                    .map(|_| ())
            })
            .unwrap();
        }
        let record = f.dispatch(&op);
        assert_eq!(record.state, OperationState::Failed, "{record:?}");
        let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
        assert!(
            read(f.store.as_ref(), |r| r.conventions().convention(
                &draft.convention_id,
                None,
                &[KnowledgeScope::Workspace]
            ))
            .is_none()
        );
        assert_eq!(
            read(f.store.as_ref(), |r| r.artifacts().list_workspace(
                None,
                &[],
                None,
                100
            ))
            .len(),
            1
        );
        assert_eq!(
            read(f.store.as_ref(), |r| r.artifacts().workspace_usage()),
            source.byte_length.get()
        );
        assert_eq!(
            read(f.store.as_ref(), |r| r.queue().status(
                devworks_core::services::conventions::indexing::commands::EFFECT_CONSUMER
            ))
            .ready,
            0
        );
    }
}
