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
    gateway::Gateway,
    ports::storage::*,
    protocol::{
        CommandReply, OperationId, OperationState,
        provider::ArtifactRef,
        records::{ConventionReadOutcome, ConventionReadResult},
        results::{DomainDecision, DomainQueryReply, OperationResult},
    },
    services::{
        UseCases,
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        conventions::{ConventionService, importing::ImportService, indexing::IndexService, model},
    },
};
use devworks_engine::dispatch;
use serde_json::json;
use std::sync::Arc;
// Compile the actual production consumer while composition is integrated by its runtime owner.
#[path = "../src/conventions_import.rs"]
mod actual_consumer;
struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    service: Arc<ImportService>,
    catalog: Arc<ConventionService>,
    ctx: InvocationContext,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let files = Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("import-cache"),
        ));
        Self::with_files(h, files)
    }
    fn with_files(h: Harness, files: Arc<dyn devworks_core::ports::files::ArtifactFiles>) -> Self {
        let store = h.open();
        let ctx = principal(store.as_ref(), "actual-human", PrincipalKind::Human);
        let artifacts = Arc::new(ArtifactService::new(
            files,
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        ));
        let catalog = Arc::new(ConventionService::new(
            store.clone(),
            artifacts.clone(),
            h.ids.clone(),
            h.clock(),
        ));
        let service = catalog.imports();
        Self {
            h,
            store,
            artifacts,
            service,
            catalog,
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
        let (a, s, actor, scope) = (
            self.artifacts.clone(),
            staged.clone(),
            self.ctx.principal().clone(),
            self.ctx
                .project_id()
                .cloned()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
        );
        let version = write(self.store.as_ref(), move |tx| {
            a.commit_host_bytes_in_scope(
                tx,
                &s,
                HostArtifactPublication {
                    title: "Uploaded portable source",
                    media_type: "application/json",
                    scope,
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
        let scope = self.ctx.project_id().map_or(
            json!({"kind":"Workspace"}),
            |project| json!({"kind":"Project","projectId":project}),
        );
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":key,"body":{"kind":"conventions.import","scope":scope,"source":reference,"schema":model::manifest_schema()}});
        match UseCases::new()
            .with_conventions(self.catalog.clone())
            .command(
                &gateway,
                &self.ctx,
                "conventions.import",
                frame.to_string().as_bytes(),
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
    fn status_reply(&self, ctx: &InvocationContext, op: &OperationId) -> DomainQueryReply {
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
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Query","queryId":"import-status","body":{"kind":"conventions.read","operationId":op,"scope":{"kind":"Workspace"}}});
        UseCases::new()
            .with_conventions(self.catalog.clone())
            .query(
                &gateway,
                ctx,
                "conventions.read",
                frame.to_string().as_bytes(),
            )
            .unwrap()
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
            catalog,
            ctx,
        } = f;
        let prior_boot = store.info().boot_id;
        drop(service);
        drop(catalog);
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

struct GateFiles {
    native: NativeArtifactFiles,
    calls: std::sync::atomic::AtomicUsize,
    started: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}
struct ReleaseGate(Arc<GateFiles>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        *self.0.release.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
macro_rules! native_files {($(fn $name:ident(&self $(,$arg:ident:$ty:ty)*) -> $out:ty;)*)=>{$(fn $name(&self,$($arg:$ty),*)->$out {self.native.$name($($arg),*)})*};}
impl devworks_core::ports::files::ArtifactFiles for GateFiles {
    native_files! {
     fn publish(&self,staging_name:&str,source:&mut dyn std::io::Read,digest:&str,bytes:u64,reuse:Option<&str>,generation:&str)->Result<devworks_core::ports::files::PublishedBlob,devworks_core::ports::files::FileFault>;
     fn managed_path(&self,digest:&str,generation:&str)->Result<String,devworks_core::ports::files::FileFault>;
     fn present(&self,digest:&str,generation:&str,bytes:u64)->Result<bool,devworks_core::ports::files::FileFault>;
     fn inspect(&self,digest:&str,generation:&str)->Result<Option<u64>,devworks_core::ports::files::FileFault>;
     fn read_range(&self,digest:&str,generation:&str,bytes:u64,offset:u64,max:usize)->Result<Vec<u8>,devworks_core::ports::files::FileFault>;
     fn open(&self,digest:&str,generation:&str,bytes:u64)->Result<Box<dyn std::io::Read+Send>,devworks_core::ports::files::FileFault>;
     fn open_controlled(&self,digest:&str,generation:&str,bytes:u64,stopped:&mut dyn FnMut()->bool)->Result<Box<dyn std::io::Read+Send>,devworks_core::ports::files::FileFault>;
     fn unlink(&self,digest:&str,generation:&str)->Result<(),devworks_core::ports::files::FileFault>;
     fn quarantine(&self,digest:&str,generation:&str)->Result<(),devworks_core::ports::files::FileFault>;
     fn staged(&self)->Result<Vec<String>,devworks_core::ports::files::FileFault>;
     fn remove_staged(&self,name:&str)->Result<(),devworks_core::ports::files::FileFault>;
     fn stored_files(&self)->Result<Vec<(String,String)>,devworks_core::ports::files::FileFault>;
     fn write_derived(&self,key:&str,bytes:&[u8])->Result<(),devworks_core::ports::files::FileFault>;
     fn read_derived(&self,key:&str,max:usize)->Result<Option<Vec<u8>>,devworks_core::ports::files::FileFault>;
     fn collect_derived(&self)->Result<u64,devworks_core::ports::files::FileFault>;
     fn export(&self,digest:&str,generation:&str,bytes:u64,destination:&std::path::Path,overwrite:bool)->Result<(),devworks_core::ports::files::FileFault>;
    }
    fn read_range_controlled(
        &self,
        digest: &str,
        generation: &str,
        bytes: u64,
        offset: u64,
        max: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, devworks_core::ports::files::FileFault> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
            self.started.send(()).unwrap();
            let mut release = self.release.lock().unwrap();
            while !*release {
                release = self.changed.wait(release).unwrap();
            }
        }
        self.native
            .read_range_controlled(digest, generation, bytes, offset, max, stopped)
    }
}
#[test]
fn cancellation_during_a_real_source_read_preserves_private_prefix_and_blocks_publication() {
    let h = Harness::new();
    let (started, waiting) = std::sync::mpsc::sync_channel(1);
    let files = Arc::new(GateFiles {
        native: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("import-cache")),
        calls: std::sync::atomic::AtomicUsize::new(0),
        started,
        release: std::sync::Mutex::new(false),
        changed: std::sync::Condvar::new(),
    });
    let _release = ReleaseGate(files.clone());
    let f = Fx::with_files(h, files.clone());
    let source = f.source(&serde_json::to_vec(&manifest(3)).unwrap());
    let op = f.admit(&source, "cancel-reading");
    let dispatcher = dispatcher_with(
        &f.h,
        f.store.clone(),
        vec![actual_consumer::consumer(f.service.clone())],
        |_| {},
    );
    let job = std::thread::spawn(move || {
        runtime().block_on(async {
            dispatcher.recover().await.unwrap();
            dispatcher.run_until_idle().await.unwrap();
        })
    });
    waiting
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("worker did not reach source read after private capture");
    let draft = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    assert_eq!(draft.phase, "Parsing");
    assert!(draft.checkpoint["offset"].as_u64().unwrap() > 0);
    assert_eq!(
        read(f.store.as_ref(), |r| r.artifacts().list_workspace(
            None,
            &[],
            None,
            50
        ))
        .len(),
        1
    );
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
    *files.release.lock().unwrap() = true;
    files.changed.notify_all();
    job.join().unwrap();
    let op_record = read(f.store.as_ref(), |r| r.operations().operation(&op)).unwrap();
    assert_eq!(
        op_record.state,
        OperationState::CancelledBeforeDispatch,
        "{op_record:?}"
    );
    assert!(
        read(f.store.as_ref(), |r| r.conventions().convention(
            &draft.convention_id,
            None,
            &[KnowledgeScope::Workspace]
        ))
        .is_none()
    );
    f.service.recover_pending().unwrap();
    assert!(
        read(f.store.as_ref(), |r| r
            .conventions()
            .convention_import_artifacts(&op, None, 32))
        .is_empty()
    );
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &f.ctx, &source, 65536)
            .is_ok()
    );
}

fn convention_only_agent(f: &Fx, name: &str) -> InvocationContext {
    let ctx = principal(f.store.as_ref(), name, PrincipalKind::Agent);
    let (actor, user) = (ctx.principal().clone(), f.ctx.clone());
    write(f.store.as_ref(), move |tx| {
        devworks_core::authority::service::bind_principal(
            tx,
            &user,
            NewPrincipalBinding {
                actor,
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::WorkspaceCoordinator),
                project_id: None,
                tool_families: Some(vec!["conventions".into()]),
                bound_by: user.principal().clone(),
            },
            None,
        )
        .map(|_| ())
        .map_err(|_| StorageError::ScopeDenied)
    })
    .unwrap();
    ctx
}
#[test]
fn convention_only_owner_status_needs_no_history_and_revoke_blocks_the_actual_worker() {
    let mut f = Fx::new();
    let human = f.ctx.clone();
    let agent = convention_only_agent(&f, "owner-agent");
    let other = convention_only_agent(&f, "different-agent");
    f.ctx = agent;
    let source = f.source(&serde_json::to_vec(&manifest(1)).unwrap());
    let op = f.admit(&source, "agent-owned-import");
    let status = match f.status_reply(&f.ctx, &op) {
        DomainQueryReply::Result {
            result:
                OperationResult::ConventionsRead {
                    value:
                        ConventionReadResult::Current(ConventionReadOutcome::Operation { status }),
                },
            ..
        } => status,
        other => panic!("{other:?}"),
    };
    assert_eq!(status.operation.operation_id, op);
    assert!(status.can_cancel);
    match f.status_reply(&other, &op) {
        DomainQueryReply::Rejected { fault, .. } => assert_eq!(
            fault.code,
            devworks_core::protocol::DomainErrorCode::NotFound
        ),
        other => panic!("foreign private status leaked: {other:?}"),
    };
    let authority = HostAuthority {
        store: f.store.as_ref(),
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: f.store.as_ref(),
        authority: &authority,
        ids: &*f.h.ids,
    };
    let frame = json!({"protocol":"1.0-draft.1","queryId":"no-history-grant","query":{"kind":"history.operation_status","operationId":op}});
    let denied = UseCases::new()
        .with_history(Arc::new(
            devworks_core::services::history::HistoryService::new(f.h.ids.clone()),
        ))
        .query(
            &gateway,
            &f.ctx,
            "history.operation_status",
            frame.to_string().as_bytes(),
        )
        .unwrap();
    match denied {
        DomainQueryReply::Rejected { fault, .. } => assert_eq!(
            fault.code,
            devworks_core::protocol::DomainErrorCode::PermissionDenied
        ),
        other => panic!("Conventions-only actor unexpectedly reached History: {other:?}"),
    };
    let (service, foreign, id, revision) = (
        f.service.clone(),
        other.clone(),
        op.clone(),
        status.revision,
    );
    assert!(
        write(f.store.as_ref(), move |tx| service
            .cancel(tx, &foreign, &id, &revision, None)
            .map_err(|_| StorageError::ScopeDenied))
        .is_err()
    );
    assert!(
        !read(f.store.as_ref(), |r| Ok(f
            .service
            .status(r, &f.ctx, &op, None)))
        .unwrap()
        .cancel_requested
    );
    let actor = f.ctx.principal().clone();
    write(f.store.as_ref(), move |tx| {
        tx.authority().revoke_principal(&actor, 1).map(|_| ())
    })
    .unwrap();
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::Failed, "{record:?}");
    assert_eq!(
        read(f.store.as_ref(), |r| r.artifacts().list_workspace(
            None,
            &[],
            None,
            50
        ))
        .len(),
        1
    );
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &human, &source, 65536)
            .is_ok()
    );
}

#[test]
fn captured_root_stop_epoch_prevents_an_admitted_import_from_publishing() {
    let mut f = Fx::new();
    let project = devworks_core::protocol::ProjectId(id("import-project"));
    let root = devworks_core::protocol::RootId(id("import-root"));
    let (p, r) = (project.clone(), root.clone());
    write(f.store.as_ref(), move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: p.clone(),
            name: "Import project".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.work().insert_work(&StoredWork {
            id: r.0.clone(),
            project_id: p,
            kind: TASK.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Owned import root".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "owned import root".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        })?;
        tx.runs()
            .insert_root_control(&StoredRootControl::initial(r))
    })
    .unwrap();
    f.ctx = f
        .ctx
        .clone()
        .with_lineage(Some(project.clone()), Some(root.clone()), None);
    let source = f.source(&serde_json::to_vec(&manifest(0)).unwrap());
    let op = f.admit(&source, "stop-after-admission");
    let captured = read(f.store.as_ref(), |r| r.operations().operation(&op)).unwrap();
    assert_eq!(captured.payload["controls"][0][1], json!(0));
    write(f.store.as_ref(), move |tx| {
        let mut control = tx.runs().root_control(&root)?.unwrap();
        let expected = control.revision;
        control.revision += 1;
        control.stop_epoch += 1;
        control.desired = DesiredExecution::Paused;
        control.updated_at_ms = tx.now_ms();
        tx.runs().update_root_control(&control, expected)
    })
    .unwrap();
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::Failed, "{record:?}");
    let d = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    assert!(
        read(f.store.as_ref(), |r| r.conventions().convention(
            &d.convention_id,
            None,
            &[KnowledgeScope::Project(project.clone())]
        ))
        .is_none()
    );
    assert_eq!(
        read(f.store.as_ref(), |r| r.artifacts().list(
            &project,
            &[],
            None,
            50
        ))
        .len(),
        1
    );
}

#[test]
fn current_parent_hold_blocks_work_owned_import_after_membership_move() {
    let mut f = Fx::new();
    let project = devworks_core::protocol::ProjectId(id("membership-project"));
    let root = devworks_core::protocol::RootId(id("membership-task"));
    let a = devworks_core::protocol::RootId(id("old-feature"));
    let b = devworks_core::protocol::RootId(id("new-feature"));
    let (p, r, old, new) = (project.clone(), root.clone(), a.clone(), b.clone());
    write(f.store.as_ref(), move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: p.clone(),
            name: "Membership project".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        let mut work = StoredWork {
            id: old.0.clone(),
            project_id: p,
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Feature".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "feature".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        };
        tx.work().insert_work(&work)?;
        work.id = new.0.clone();
        tx.work().insert_work(&work)?;
        work.id = r.0.clone();
        work.kind = TASK.into();
        work.parent_feature_id = Some(old.0.clone());
        tx.work().insert_work(&work)?;
        for root in [r, old, new] {
            tx.runs()
                .insert_root_control(&StoredRootControl::initial(root))?;
        }
        Ok(())
    })
    .unwrap();
    f.ctx = f
        .ctx
        .clone()
        .with_lineage(Some(project.clone()), Some(root.clone()), None);
    let source = f.source(&serde_json::to_vec(&manifest(0)).unwrap());
    let op = f.admit(&source, "move-before-publication");
    let before = read(f.store.as_ref(), |r| r.operations().operation(&op)).unwrap();
    assert_eq!(before.payload["controls"][1][0], json!(a));
    write(f.store.as_ref(), move |tx| {
        let mut work = tx.work().work(&root.0)?.unwrap();
        let expected = work.revision;
        work.parent_feature_id = Some(b.0.clone());
        work.revision += 1;
        work.scope_revision += 1;
        tx.work().update_work(&work, expected)?;
        let mut control = tx.runs().root_control(&b)?.unwrap();
        let expected = control.revision;
        control.revision += 1;
        control.stop_epoch += 1;
        control.desired = DesiredExecution::Paused;
        control.updated_at_ms = tx.now_ms();
        tx.runs().update_root_control(&control, expected)
    })
    .unwrap();
    let record = f.dispatch(&op);
    assert_eq!(record.state, OperationState::Failed, "{record:?}");
    let d = read(f.store.as_ref(), |r| r.conventions().convention_import(&op)).unwrap();
    assert!(
        read(f.store.as_ref(), |r| r.conventions().convention(
            &d.convention_id,
            None,
            &[KnowledgeScope::Project(project.clone())]
        ))
        .is_none()
    );
}
#[path = "conventions_import/service.rs"]
mod service;

#[path = "conventions_import/waiting_controls.rs"]
mod waiting_controls;
