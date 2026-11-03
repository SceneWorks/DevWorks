//! The artifact store end to end on the real SQLite store and real managed files: publication
//! through the gateway with receipts, versions, origin links, scope and reviewer/tool policy,
//! crash recovery at every publication step, tombstone-before-unlink with shared blobs, backup
//! pins and readers, quotas, separately authorized export destinations, refused source links
//! and the retained GetAttachment mapping. Deterministic temporary directories; no inference.
use super::{Harness, T0};
use crate::fs::NativeFilesystem;
use crate::fs::artifacts::{NativeArtifactFiles, Step};
use crate::storage::sqlite::{MIGRATION_0006_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::destructive::BlockReason;
use devworks_core::authority::reads::ReadRoot;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::gateway::Gateway;
use devworks_core::ports::files::{
    ArtifactFiles, ArtifactSources, FileFault, ResolvedSource, SourceOrigin, SourceRefusal,
};
use devworks_core::ports::storage::*;
use devworks_core::ports::{AuthorityPort, CommandHandler, Preparation, Prepared};
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::provider::{ArtifactRef, Digest};
use devworks_core::protocol::records::{ArtifactReadContent, ArtifactRecord, MutationSummary};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::shared::Subject;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, OperationState, ProjectId,
    Rejection, Revision, RootId,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::lifecycle::{
    ExportRefusal, RecoveryReport, StreamRefusal, VersionResolution, pin_for_backup, release_backup,
};
use devworks_core::services::artifacts::{
    ArtifactService, INTEGRITY_TARGET, QUOTA_TARGET, SHARED_REFERENCES_REMAIN, STAGE_CAPABILITY,
    StageRequest, StagedFile, Staging, version_ref, versions_digest,
};
use devworks_core::settings::catalog::{AgentRole, descriptor};
use devworks_core::settings::service::SettingsService;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}

/// Host source handles: upload tickets, attachments and work-output handles mapped to files
/// beneath one registered read root.
/// A callback run once while a handle is resolved (inside the save's preparation).
type OnResolve = Box<dyn FnOnce() + Send>;

struct Sources {
    root: ReadRoot,
    handles: Mutex<HashMap<String, (String, SourceOrigin)>>,
    on_resolve: Mutex<HashMap<String, OnResolve>>,
}
impl ArtifactSources for Sources {
    fn resolve(
        &self,
        _: &InvocationContext,
        source: &ArtifactSource,
        _: &ProjectId,
    ) -> Result<ResolvedSource, SourceRefusal> {
        let key = match source {
            ArtifactSource::Upload { ticket_id } => ticket_id,
            ArtifactSource::Attachment { attachment_id } => attachment_id,
            ArtifactSource::WorkOutput { file_handle } => file_handle,
        };
        if key.as_str() == "denied" {
            return Err(SourceRefusal::PermissionDenied);
        }
        let hook = self.on_resolve.lock().unwrap().remove(key.as_str());
        if let Some(hook) = hook {
            hook();
        }
        let handles = self.handles.lock().unwrap();
        let (relative, origin) = handles.get(key.as_str()).ok_or(SourceRefusal::NotFound)?;
        Ok(ResolvedSource {
            root: self.root.clone(),
            relative: relative.clone(),
            origin: origin.clone(),
        })
    }
}

const ACTORS: [(&str, PrincipalKind); 6] = [
    ("user-a", PrincipalKind::Human),
    ("agent-a", PrincipalKind::Agent),
    ("agent-p", PrincipalKind::Agent),
    ("reviewer-a", PrincipalKind::Agent),
    ("agent-t", PrincipalKind::Agent),
    ("agent-b", PrincipalKind::Agent),
];

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    files: Arc<NativeArtifactFiles>,
    svc: Arc<ArtifactService>,
    cases: UseCases,
    sources: Arc<Sources>,
    n: Arc<AtomicU64>,
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let src = h.root.0.with_extension("sources");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).unwrap();
        let sources = Arc::new(Sources {
            root: ReadRoot {
                identity: crate::fs::identity_of(&std::fs::metadata(&src).unwrap()),
                location: src,
            },
            handles: Mutex::new(HashMap::new()),
            on_resolve: Mutex::new(HashMap::new()),
        });
        let fx = Self::open(h, sources, Arc::new(AtomicU64::new(0)));
        fx.store
            .with_writer_connection(|c| {
                super::super::conn::db(c.execute_batch(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);
                     INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Feature A',1,0,1,1,1,'{}');
                     INSERT INTO work_items VALUES('task-a','project-a','task','root-a',NULL,NULL,'Task A',1,0,1,1,1,'{}');
                     INSERT INTO work_items VALUES('root-b','project-b','feature',NULL,NULL,NULL,'Feature B',1,0,1,1,1,'{}');",
                ))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in ACTORS {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.ctx("user-a", PrincipalKind::Human, None);
        for (who, role, project, tools) in [
            ("agent-a", AgentRole::Worker, None, None),
            ("agent-p", AgentRole::Worker, Some("project-a"), None),
            ("reviewer-a", AgentRole::CodeReviewer, None, None),
            (
                "agent-t",
                AgentRole::Worker,
                None,
                Some(vec!["settings".to_owned()]),
            ),
            ("agent-b", AgentRole::Worker, Some("project-b"), None),
        ] {
            let (by, binding) = (
                user.clone(),
                NewPrincipalBinding {
                    actor: actor(who),
                    kind: PrincipalKind::Agent,
                    role: Some(role),
                    project_id: project.map(self::project),
                    tool_families: tools,
                    bound_by: actor("user-a"),
                },
            );
            fx.write(move |tx| {
                auth::bind_principal(tx, &by, binding, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
        fx
    }

    /// Opens (or reopens, after a simulated crash) the store and the service over the same data
    /// root and source handles.
    fn open(h: Harness, sources: Arc<Sources>, n: Arc<AtomicU64>) -> Self {
        let store = Arc::new(h.store());
        let files = Arc::new(NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.with_extension("cache"),
        ));
        let svc = Arc::new(ArtifactService::new(
            files.clone(),
            sources.clone(),
            Arc::new(NativeFilesystem::new()),
            h.clock.clone(),
            h.ids.clone(),
        ));
        let settings = Arc::new(SettingsService {
            roles: Arc::new(devworks_core::authority::HostRoles {
                store: store.clone(),
            }),
            paths: Arc::new(crate::paths::FsPathProbe),
            ids: h.ids.clone(),
        });
        let cases = UseCases::new()
            .with_artifacts(svc.clone())
            .with_settings(settings);
        Self {
            h,
            store,
            files,
            svc,
            cases,
            sources,
            n,
        }
    }

    /// Simulates a process restart: every handle on the store is dropped, then it is reopened.
    fn restart(self) -> Self {
        let Fx {
            h,
            store,
            files,
            svc,
            cases,
            sources,
            n,
        } = self;
        drop((cases, svc, files, store));
        Self::open(h, sources, n)
    }

    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn ctx(&self, who: &str, kind: PrincipalKind, project: Option<&str>) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store)
            .context(
                actor(who),
                kind,
                id("invocation-1"),
                id("ui-main"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(project.map(self::project), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
    }
    /// An agent working on `root-a` in `project-a` within session `session-1`.
    fn worker(&self, who: &str) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent, Some("project-a"))
            .with_lineage(Some(project("project-a")), Some(RootId(id("root-a"))), None)
            .with_session(id("session-1"))
    }
    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> T {
        run_write(&*self.store, WriteOptions::default(), f).expect("write")
    }
    fn read<T>(&self, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
        read_snapshot(&*self.store, &ReadOptions::default(), f).expect("read")
    }
    fn key(&self) -> String {
        format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst))
    }

    /// Writes a source file and registers `handle` for it.
    fn source(&self, handle: &str, relative: &str, bytes: &[u8], origin: SourceOrigin) {
        let path = self.sources.root.location.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        self.sources
            .handles
            .lock()
            .unwrap()
            .insert(handle.to_owned(), (relative.to_owned(), origin));
    }

    fn command_with(
        &self,
        ctx: &InvocationContext,
        op: &str,
        frame: Value,
        options: WriteOptions,
    ) -> DomainCommandReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command_with(&gateway, ctx, op, frame.to_string().as_bytes(), options)
            .expect("addressable")
    }
    fn command(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainCommandReply {
        self.command_with(ctx, op, frame, WriteOptions::default())
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        op: &str,
        query: Value,
        cursor: Option<&str>,
    ) -> DomainQueryReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        let mut frame =
            json!({"protocol": "1.0-draft.1", "queryId": "q-1", "query": query, "limit": 50});
        if let Some(c) = cursor {
            frame["cursor"] = json!(c);
        }
        self.cases
            .query(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }

    fn save_frame(
        &self,
        key: &str,
        project: &str,
        title: &str,
        media: &str,
        source: Value,
        bytes: &[u8],
    ) -> Value {
        json!({"protocol": "1.0-draft.1", "requestKey": key, "command": {
            "kind": "Save", "projectId": project, "title": title, "mediaType": media,
            "source": source, "expectedDigest": sha256_hex(bytes),
            "expectedBytes": bytes.len().to_string()}})
    }
    /// Saves `bytes` (already registered under `handle`) and returns the reply.
    fn save(
        &self,
        ctx: &InvocationContext,
        project: &str,
        title: &str,
        media: &str,
        handle: &str,
        bytes: &[u8],
    ) -> DomainCommandReply {
        let frame = self.save_frame(
            &self.key(),
            project,
            title,
            media,
            json!({"kind": "Upload", "ticketId": handle}),
            bytes,
        );
        self.command(ctx, "artifacts.save", frame)
    }
    /// Registers a source and saves it as the user in `project-a`.
    fn saved(&self, handle: &str, title: &str, media: &str, bytes: &[u8]) -> ArtifactRecord {
        self.source(
            handle,
            &format!("{handle}.bin"),
            bytes,
            SourceOrigin::default(),
        );
        saved(self.save(&self.user(), "project-a", title, media, handle, bytes))
    }
    fn delete(&self, ctx: &InvocationContext, deletion: Value) -> DomainCommandReply {
        self.command(
            ctx,
            "artifacts.delete",
            json!({"protocol": "1.0-draft.1", "requestKey": self.key(), "deletion": deletion}),
        )
    }
    fn reference(
        &self,
        ctx: &InvocationContext,
        r: &ArtifactRef,
        subject: Value,
        expected: i64,
    ) -> DomainCommandReply {
        self.command(
            ctx,
            "artifacts.reference",
            json!({"protocol": "1.0-draft.1", "requestKey": self.key(), "command": {
                "kind": "Reference", "artifactId": r.artifact_id, "versionId": r.version_id,
                "subject": subject, "expectedRevision": expected.to_string()}}),
        )
    }
    fn artifact(&self, aid: &Id) -> StoredArtifact {
        let aid = aid.clone();
        self.read(|s| s.artifacts().artifact(&aid)).unwrap()
    }
    fn blob(&self, digest: &str) -> Option<BlobRecord> {
        self.read(|s| s.artifacts().blob(digest))
    }
    fn blob_file(&self, digest: &str) -> PathBuf {
        self.h
            .root
            .0
            .join(format!("artifacts/sha256/{}/{digest}", &digest[..2]))
    }
    fn events(&self) -> Vec<EventRecord> {
        self.read(|s| {
            s.history()
                .page(&EventFilter::All, 0, 500)
                .map(|p| p.events)
        })
    }
    /// Every staging operation in creation order, whatever history references it.
    fn staging_ops_all(&self) -> Vec<OperationRecord> {
        let ids = self
            .store
            .with_writer_connection(|c| {
                let mut s = super::super::conn::db(c.prepare(
                    "SELECT id FROM operations WHERE capability=?1 ORDER BY created_at,rowid",
                ))?;
                let rows = super::super::conn::db(
                    s.query_map([STAGE_CAPABILITY], |r| r.get::<_, String>(0)),
                )?;
                super::super::conn::db(rows.collect::<Result<Vec<_>, _>>())
            })
            .unwrap();
        ids.into_iter()
            .map(|i| {
                let op = devworks_core::protocol::OperationId(id(&i));
                self.read(|s| s.operations().operation(&op)).unwrap()
            })
            .collect()
    }
    fn staging_ops(&self) -> Vec<OperationRecord> {
        let mut out = Vec::new();
        for e in self.events() {
            if let Some(op) = e.operation_id
                && let Some(rec) = self.read(|s| s.operations().operation(&op))
                && rec.capability == STAGE_CAPABILITY
                && !out.iter().any(|o: &OperationRecord| o.id == rec.id)
            {
                out.push(rec);
            }
        }
        out
    }
}

fn recorded(reply: DomainCommandReply) -> (DomainDecision, bool) {
    match reply {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => (receipt.decision, replayed),
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
fn applied(reply: DomainCommandReply) -> OperationResult {
    match recorded(reply).0 {
        DomainDecision::Applied { result } | DomainDecision::NoChange { result } => result,
        other => panic!("expected an applied decision, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply).0 {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn rejected(reply: DomainCommandReply) -> Rejection {
    match reply {
        CommandReply::Rejected { code, .. } => code,
        other => panic!("expected a rejection, got {other:?}"),
    }
}
fn saved(reply: DomainCommandReply) -> ArtifactRecord {
    match applied(reply) {
        OperationResult::ArtifactsSave { value } => value,
        other => panic!("{other:?}"),
    }
}
fn summary(reply: DomainCommandReply) -> MutationSummary {
    match applied(reply) {
        OperationResult::ArtifactsReference { value }
        | OperationResult::ArtifactsDelete { value } => value,
        other => panic!("{other:?}"),
    }
}
fn result(reply: DomainQueryReply) -> OperationResult {
    match reply {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("expected a result, got {other:?}"),
    }
}
fn fault(reply: DomainQueryReply) -> DomainFault {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}
fn hex(d: &Digest) -> String {
    String::from(d.clone())
}
fn target(f: &DomainFault) -> &str {
    f.target_kind.as_ref().map_or("", |t| t.0.as_str())
}
fn version_query(kind: &str, r: &ArtifactRef) -> Value {
    json!({"kind": kind, "artifactId": r.artifact_id, "versionId": r.version_id})
}
fn delete_version(r: &ArtifactRef, revision: i64, reference: i64) -> Value {
    json!({"kind": "DeleteVersion", "artifactId": r.artifact_id, "versionId": r.version_id,
        "expectedRevision": revision.to_string(), "expectedReferenceRevision": reference.to_string()})
}

#[test]
fn save_list_read_preview_reference_export_and_delete_through_the_gateway() {
    let fx = Fx::new();
    let text = b"# Findings\n\nAll checks pass.\n";
    fx.source(
        "out-1",
        "worktree/report.md",
        text,
        SourceOrigin {
            repository_id: Some(id("repo-a")),
            repository_revision: Some("0123abcd".into()),
            original_name: Some("report.md".into()),
        },
    );
    let worker = fx.worker("agent-a");
    let frame = fx.save_frame(
        "save-1",
        "project-a",
        "Findings",
        "text/markdown",
        json!({"kind": "WorkOutput", "fileHandle": "out-1"}),
        text,
    );
    let record = saved(fx.command(&worker, "artifacts.save", frame.clone()));
    let r = record.artifact.clone();
    assert_eq!(hex(&r.sha256), sha256_hex(text));
    assert_eq!(r.byte_length.get(), text.len() as i64);
    assert_eq!(record.name.0, "Findings");
    // The same command replays its receipt and stages nothing new.
    let (decision, replayed) = recorded(fx.command(&worker, "artifacts.save", frame.clone()));
    // The replayed receipt carries the record byte for byte (redaction leaves names and digests).
    match decision {
        DomainDecision::Applied {
            result: OperationResult::ArtifactsSave { value },
        } if replayed => {
            assert_eq!(
                serde_json::to_value(&value).unwrap(),
                serde_json::to_value(&record).unwrap()
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.staging_ops().len(), 1);
    assert_eq!(fx.staging_ops()[0].state, OperationState::ObservedComplete);
    // A reused key with different content is an idempotency conflict.
    let mut changed = frame;
    changed["command"]["title"] = json!("Other");
    assert_eq!(
        rejected(fx.command(&worker, "artifacts.save", changed)),
        Rejection::IdempotencyConflict
    );

    // Provenance: producing agent, session, origin work and repository revision.
    let stored = fx.artifact(&r.artifact_id);
    assert_eq!(stored.origin_work_id, Some(RootId(id("root-a"))));
    assert_eq!(stored.provenance["producedBy"], "agent-a");
    assert_eq!(stored.provenance["sessionId"], "session-1");
    assert_eq!(stored.provenance["repositoryRevision"], "0123abcd");

    // Listed in its project, with a cursor-bound page.
    let OperationResult::ArtifactsList { value } = result(fx.query(
        &worker,
        "artifacts.list",
        json!({"kind": "List", "projectId": "project-a"}),
        None,
    )) else {
        panic!()
    };
    assert_eq!(value.items.0.len(), 1);
    assert_eq!(value.items.0[0].artifact.version_id, r.version_id);
    assert!(!value.has_more);

    // Read: bounded inline text.
    match result(fx.query(&worker, "artifacts.read", version_query("Read", &r), None)) {
        OperationResult::ArtifactsRead {
            value:
                ArtifactReadContent::Text {
                    content,
                    truncated,
                    next_byte,
                    ..
                },
        } => {
            assert_eq!(content.0.as_bytes(), text);
            assert!(!truncated);
            assert_eq!(next_byte.get(), text.len() as i64);
        }
        other => panic!("{other:?}"),
    }
    // Preview: a derived text preview whose own ID is readable under the same access rules.
    let OperationResult::ArtifactsPreview { value: p } = result(fx.query(
        &worker,
        "artifacts.preview",
        version_query("Preview", &r),
        None,
    )) else {
        panic!()
    };
    assert!(p.preview.version_id.as_str().starts_with("pv-"));
    assert_eq!(p.artifact.version_id, r.version_id);
    match result(fx.query(
        &worker,
        "artifacts.read",
        version_query("Read", &p.preview),
        None,
    )) {
        OperationResult::ArtifactsRead {
            value: ArtifactReadContent::Text { content, .. },
        } => {
            assert_eq!(content.0.as_bytes(), text)
        }
        other => panic!("{other:?}"),
    }
    // Derived previews are a separate collectible class: collecting them leaves the artifact.
    assert!(fx.svc.collect_derived().unwrap() >= 1);
    assert!(matches!(
        result(fx.query(&worker, "artifacts.read", version_query("Read", &r), None)),
        OperationResult::ArtifactsRead { .. }
    ));

    // Reference to a task: navigation both ways, origin preserved.
    let s = summary(fx.reference(&worker, &r, json!({"kind": "root", "id": "task-a"}), 1));
    assert!(s.changed && s.revision.get() == 2);
    let again = summary(fx.reference(&worker, &r, json!({"kind": "root", "id": "task-a"}), 2));
    assert!(!again.changed, "an identical reference is a no-change");
    let stale = refused(fx.reference(&worker, &r, json!({"kind": "root", "id": "root-a"}), 1));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
    let linked = fx.read(|s| {
        fx.svc
            .linked(s, &worker, &Subject::Root(RootId(id("task-a"))))
    });
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].0.id, r.artifact_id);
    let origin = fx.read(|s| {
        fx.svc
            .linked(s, &worker, &Subject::Root(RootId(id("root-a"))))
    });
    assert_eq!(origin.len(), 1, "the origin link survives reuse");
    let detail = fx
        .read(|s| fx.svc.detail(s, &worker, &r.artifact_id))
        .unwrap();
    assert_eq!(detail.artifact.origin_work_id, Some(RootId(id("root-a"))));
    // Work history navigates to the artifact (root-scoped history events).
    let by_root = fx.read(|s| {
        s.history()
            .page(&EventFilter::Root(RootId(id("task-a"))), 0, 50)
            .map(|p| p.events)
    });
    assert!(
        by_root
            .iter()
            .any(|e| e.kind == "ArtifactReferenced" && e.aggregate_id == r.artifact_id.as_str())
    );
    let OperationResult::ArtifactsList { value } = result(fx.query(
        &worker,
        "artifacts.list",
        json!({"kind": "List", "projectId": "project-a"}),
        None,
    )) else {
        panic!()
    };
    assert_eq!(value.items.0[0].reference_count.get(), 1);

    // Export: a stream handle for the caller, with the original name.
    let OperationResult::ArtifactsExport { value: export } = result(fx.query(
        &worker,
        "artifacts.export",
        version_query("Export", &r),
        None,
    )) else {
        panic!()
    };
    assert_eq!(export.suggested_name.0, "report.md");
    let authority = fx.authority();
    let mut stream = fx
        .svc
        .open_stream(&authority, &*fx.store, &worker, &export.stream_id)
        .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, text);
    drop(stream);
    // Another principal cannot use the handle.
    assert!(matches!(
        fx.svc
            .open_stream(&authority, &*fx.store, &fx.user(), &export.stream_id),
        Err(StreamRefusal::NotFound)
    ));

    // Delete the version: tombstoned first; new reads fail at once.
    let a = fx.artifact(&r.artifact_id);
    let d = summary(fx.delete(
        &worker,
        delete_version(&r, a.revision, a.reference_revision),
    ));
    assert!(d.changed);
    assert_eq!(
        fault(fx.query(&worker, "artifacts.read", version_query("Read", &r), None)).code,
        DomainErrorCode::NotFound
    );
    assert!(matches!(
        fx.svc
            .open_stream(&authority, &*fx.store, &worker, &export.stream_id),
        Err(StreamRefusal::NotFound)
    ));
    assert_eq!(
        fx.blob(&hex(&r.sha256)).unwrap().state,
        BlobState::UnlinkPending
    );
    assert!(
        fx.blob_file(&hex(&r.sha256)).exists(),
        "unlink follows the tombstone"
    );
    let report = fx.svc.collect(&*fx.store).unwrap();
    assert_eq!(report.unlinked, vec![hex(&r.sha256)]);
    assert!(!fx.blob_file(&hex(&r.sha256)).exists());
    assert_eq!(fx.blob(&hex(&r.sha256)).unwrap().state, BlobState::Unlinked);
    let kinds: Vec<String> = fx.events().into_iter().map(|e| e.kind).collect();
    for k in [
        "ArtifactVersionPublished",
        "ArtifactReferenced",
        "ArtifactVersionDeleted",
        "ArtifactBlobUnlinked",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} in {kinds:?}");
    }
    // History is attributed with actor, scope and the staging operation.
    let published = fx
        .events()
        .into_iter()
        .find(|e| e.kind == "ArtifactVersionPublished")
        .unwrap();
    assert_eq!(published.actor, actor("agent-a"));
    assert_eq!(published.project_id, Some(project("project-a")));
    assert_eq!(published.root_id, Some(RootId(id("root-a"))));
    assert!(published.operation_id.is_some());
    assert_eq!(published.payload["repositoryRevision"], "0123abcd");
    assert_eq!(published.payload["digest"], sha256_hex(text));
    assert_eq!(published.payload["title"], "Findings");
    assert_eq!(published.payload["source"]["fileHandle"], "out-1");
}

fn as_failure<T>(
    r: Result<T, devworks_core::services::artifacts::Failure>,
) -> Result<T, StorageError> {
    r.map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
}
fn list(fx: &Fx, ctx: &InvocationContext, p: &str) -> Vec<ArtifactRecord> {
    match result(fx.query(
        ctx,
        "artifacts.list",
        json!({"kind": "List", "projectId": p}),
        None,
    )) {
        OperationResult::ArtifactsList { value } => value.items.0,
        other => panic!("{other:?}"),
    }
}
fn read_text(fx: &Fx, ctx: &InvocationContext, r: &ArtifactRef) -> String {
    match result(fx.query(ctx, "artifacts.read", version_query("Read", r), None)) {
        OperationResult::ArtifactsRead {
            value: ArtifactReadContent::Text { content, .. },
        } => content.0,
        other => panic!("{other:?}"),
    }
}

#[test]
fn versions_are_never_overwritten_and_exact_references_survive() {
    let fx = Fx::new();
    let first = fx.saved("t1", "Notes", "text/plain", b"one").artifact;
    let second = fx.saved("t2", "Notes", "text/plain", b"two").artifact;
    assert_ne!(
        first.artifact_id, second.artifact_id,
        "a save never overwrites"
    );
    // The documents replacement seam appends version 2 to the first artifact.
    let revised = b"one, revised";
    fx.source("t3", "t3.bin", revised, SourceOrigin::default());
    let user = fx.user();
    let env = Preparation::new(&*fx.store, None, None);
    let digest = Digest::try_from(sha256_hex(revised)).unwrap();
    let source = ArtifactSource::Upload {
        ticket_id: id("t3"),
    };
    let request = StageRequest {
        project: &project("project-a"),
        source: &source,
        expected_digest: &digest,
        expected_bytes: revised.len() as i64,
        request_digest: "replace-1",
    };
    let staged = match fx.svc.stage(&env, &user, &request).unwrap() {
        Staging::Staged(s) => s,
        other => panic!("{other:?}"),
    };
    let (svc, aid, ctx, stale) = (
        fx.svc.clone(),
        first.artifact_id.clone(),
        user.clone(),
        staged.clone(),
    );
    // A stale expected revision is a conflict and publishes nothing.
    let conflict = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        as_failure(svc.publish_version(
            tx,
            &ctx,
            &aid,
            &Revision::new(7).unwrap(),
            "text/plain",
            &stale,
        ))
    });
    assert!(conflict.is_err());
    let (svc, aid, ctx) = (fx.svc.clone(), first.artifact_id.clone(), user.clone());
    let v2 = fx.write(move |tx| {
        as_failure(svc.publish_version(
            tx,
            &ctx,
            &aid,
            &Revision::new(1).unwrap(),
            "text/plain",
            &staged,
        ))
    });
    assert_eq!(v2.version_no, 2);
    let v2ref = version_ref(&v2);
    let items = list(&fx, &user, "project-a");
    let current = items
        .iter()
        .find(|i| i.artifact.artifact_id == first.artifact_id)
        .unwrap();
    assert_eq!(
        current.artifact.version_id, v2.id,
        "the replacement is current"
    );
    // The exact old reference still reads its own bytes and resolves to itself.
    assert_eq!(read_text(&fx, &user, &first), "one");
    assert_eq!(read_text(&fx, &user, &v2ref), "one, revised");
    let resolve = |r: &ArtifactRef| fx.read(|s| fx.svc.resolve_version(s, &user, r));
    assert!(matches!(resolve(&first), VersionResolution::Available(v) if v.version_no == 1));
    let mut wrong = first.clone();
    wrong.sha256 = second.sha256.clone();
    assert_eq!(
        resolve(&wrong),
        VersionResolution::NotFound,
        "never redirected"
    );
    // Deleting the current version does not reactivate the older one.
    let a = fx.artifact(&first.artifact_id);
    summary(fx.delete(
        &user,
        delete_version(&v2ref, a.revision, a.reference_revision),
    ));
    assert_eq!(fx.artifact(&first.artifact_id).current_version_id, None);
    assert!(
        list(&fx, &user, "project-a")
            .iter()
            .all(|i| i.artifact.artifact_id != first.artifact_id)
    );
    assert!(matches!(
        resolve(&v2ref),
        VersionResolution::Unavailable { .. }
    ));
    assert!(
        matches!(resolve(&first), VersionResolution::Available(_)),
        "retained until explicitly deleted"
    );
    // Explicitly deleting the last version deletes the artifact; the tombstone keeps provenance.
    let a = fx.artifact(&first.artifact_id);
    summary(fx.delete(
        &user,
        delete_version(&first, a.revision, a.reference_revision),
    ));
    assert_eq!(
        fx.artifact(&first.artifact_id).state,
        ArtifactState::Deleted
    );
    match resolve(&first) {
        VersionResolution::Unavailable { version, .. } => {
            assert_eq!(version.blob_digest, hex(&first.sha256))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn project_scope_reviewers_and_tool_selection_are_enforced() {
    let fx = Fx::new();
    let r = fx.saved("t1", "Doc", "text/plain", b"hello").artifact;
    fx.source("t2", "t2.bin", b"x", SourceOrigin::default());
    let list_a = json!({"kind": "List", "projectId": "project-a"});
    // Code reviewers are read-only: they read but never save.
    let reviewer = fx.ctx("reviewer-a", PrincipalKind::Agent, None);
    assert_eq!(
        rejected(fx.save(&reviewer, "project-a", "x", "text/plain", "t2", b"x")),
        Rejection::PermissionDenied
    );
    assert_eq!(read_text(&fx, &reviewer, &r), "hello");
    // An agent whose selected tools exclude artifacts is refused.
    let tools = fx.ctx("agent-t", PrincipalKind::Agent, None);
    assert_eq!(
        fault(fx.query(&tools, "artifacts.list", list_a.clone(), None)).code,
        DomainErrorCode::PermissionDenied
    );
    // A principal confined to project-b neither sees nor writes project-a artifacts.
    let other = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    assert_eq!(
        fault(fx.query(&other, "artifacts.read", version_query("Read", &r), None)).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        fault(fx.query(&other, "artifacts.list", list_a.clone(), None)).code,
        DomainErrorCode::PermissionDenied
    );
    let f = refused(fx.save(&other, "project-a", "x", "text/plain", "t2", b"x"));
    assert_eq!(
        (f.code, target(&f)),
        (DomainErrorCode::PermissionDenied, "project")
    );
    assert_eq!(fx.staging_ops().len(), 1, "a refused save stages nothing");
    // A confined principal invoked outside its project is denied by current authority.
    let outside = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-a"));
    assert_eq!(
        fault(fx.query(&outside, "artifacts.list", list_a, None)).code,
        DomainErrorCode::PermissionDenied
    );
    // References only point at work the artifact's projects can see.
    let worker = fx.ctx("agent-a", PrincipalKind::Agent, None);
    let f = refused(fx.reference(&worker, &r, json!({"kind": "root", "id": "root-b"}), 1));
    assert_eq!(f.code, DomainErrorCode::NotFound);
}

#[test]
fn a_crash_at_any_publication_step_is_reconciled_without_visible_metadata() {
    for step in [
        Some(Step::Written),
        Some(Step::Flushed),
        Some(Step::Renamed),
        None,
    ] {
        let fx = Fx::new();
        let bytes = format!("payload {step:?}").into_bytes();
        let digest = sha256_hex(&bytes);
        fx.source("t1", "t1.bin", &bytes, SourceOrigin::default());
        let user = fx.user();
        match step {
            Some(s) => {
                *fx.files.crash_at.lock().unwrap() = Some(s);
                let crashed = catch_unwind(AssertUnwindSafe(|| {
                    fx.save(&user, "project-a", "Crash", "text/plain", "t1", &bytes)
                }));
                assert!(crashed.is_err(), "{s:?}");
            }
            None => {
                // The bytes are published, then the process dies before the unit of work.
                let env = Preparation::new(&*fx.store, None, None);
                let expected = Digest::try_from(digest.clone()).unwrap();
                let source = ArtifactSource::Upload {
                    ticket_id: id("t1"),
                };
                let staged = fx
                    .svc
                    .stage(
                        &env,
                        &user,
                        &StageRequest {
                            project: &project("project-a"),
                            source: &source,
                            expected_digest: &expected,
                            expected_bytes: bytes.len() as i64,
                            request_digest: "crash",
                        },
                    )
                    .unwrap();
                assert!(matches!(staged, Staging::Staged(_)));
                assert!(fx.blob_file(&digest).exists());
            }
        }
        assert!(
            fx.blob(&digest).is_none(),
            "metadata never names an unpublished file"
        );
        let fx = fx.restart();
        let user = fx.user();
        let report = fx.svc.recover(&*fx.store).unwrap();
        assert_eq!(report.failed.len(), 1, "{step:?}: {report:?}");
        match step {
            Some(Step::Written) | Some(Step::Flushed) => {
                assert_eq!(report.removed_staging.len(), 1, "{report:?}")
            }
            _ => assert_eq!(report.removed_orphans, vec![digest.clone()], "{report:?}"),
        }
        assert!(fx.files.staged().unwrap().is_empty());
        assert!(!fx.blob_file(&digest).exists());
        let failed = report.failed[0].clone();
        let op = fx.read(|s| s.operations().operation(&failed)).unwrap();
        assert_eq!(op.state, OperationState::Failed);
        assert!(
            list(&fx, &user, "project-a").is_empty(),
            "nothing became visible"
        );
        // Recovery is idempotent and the same content saves normally afterwards.
        assert_eq!(fx.svc.recover(&*fx.store).unwrap(), Default::default());
        let r = saved(fx.save(&user, "project-a", "Retry", "text/plain", "t1", &bytes)).artifact;
        assert_eq!(hex(&r.sha256), digest);
        assert!(fx.blob_file(&digest).exists());
    }
}

#[test]
fn deletion_tombstones_first_and_unlinks_only_unreferenced_unpinned_unread_blobs() {
    let fx = Fx::new();
    let user = fx.user();
    let bytes = b"\x00\x01binary evidence";
    let a1 = fx
        .saved("t1", "Evidence", "application/octet-stream", bytes)
        .artifact;
    let a2 = fx
        .saved("t2", "Evidence copy", "application/octet-stream", bytes)
        .artifact;
    let d = hex(&a1.sha256);
    assert_eq!(d, hex(&a2.sha256));
    assert_ne!(
        a1.artifact_id, a2.artifact_id,
        "deduplication shares bytes, not identity"
    );
    // Deleting one artifact keeps the bytes another surviving version needs.
    let s = fx.artifact(&a1.artifact_id);
    summary(fx.delete(&user, delete_version(&a1, s.revision, s.reference_revision)));
    assert_eq!(fx.blob(&d).unwrap().state, BlobState::Present);
    assert_eq!(fx.svc.collect(&*fx.store).unwrap(), Default::default());
    assert!(fx.blob_file(&d).exists());
    assert_eq!(
        fault(fx.query(&user, "artifacts.read", version_query("Read", &a1), None)).code,
        DomainErrorCode::NotFound
    );
    // An admitted reader of the surviving artifact.
    let OperationResult::ArtifactsRead {
        value:
            ArtifactReadContent::Binary {
                stream_id,
                byte_length,
                ..
            },
    } = result(fx.query(&user, "artifacts.read", version_query("Read", &a2), None))
    else {
        panic!()
    };
    assert_eq!(byte_length.get(), bytes.len() as i64);
    let authority = fx.authority();
    let mut reader = fx
        .svc
        .open_stream(&authority, &*fx.store, &user, &stream_id)
        .unwrap();
    // Whole-artifact deletion binds the exact version set.
    let s = fx.artifact(&a2.artifact_id);
    let whole = |digest: String| {
        json!({"kind": "DeleteArtifact", "artifactId": a2.artifact_id,
        "expectedRevision": s.revision.to_string(),
        "expectedReferenceRevision": s.reference_revision.to_string(),
        "expectedVersionsDigest": digest})
    };
    let f = refused(fx.delete(&user, whole(sha256_hex(b"stale"))));
    assert_eq!(
        (f.code, target(&f)),
        (DomainErrorCode::Conflict, "artifact_versions")
    );
    let aid = a2.artifact_id.clone();
    let versions = fx.read(|s| s.artifacts().versions(&aid));
    summary(fx.delete(&user, whole(versions_digest(&versions))));
    assert_eq!(fx.blob(&d).unwrap().state, BlobState::UnlinkPending);
    // A backup pin defers removal.
    let holder = id("backup-1");
    let pinned = fx.write(move |tx| pin_for_backup(tx, &holder));
    assert!(pinned.iter().any(|b| b.digest == d));
    let report = fx.svc.collect(&*fx.store).unwrap();
    assert_eq!(report.deferred_pinned, vec![d.clone()]);
    let holder = id("backup-1");
    assert_eq!(
        fx.write(move |tx| release_backup(tx, &holder)),
        pinned.len() as i64
    );
    // The admitted reader defers removal and still finishes its read.
    assert_eq!(
        fx.svc.collect(&*fx.store).unwrap().deferred_reading,
        vec![d.clone()]
    );
    assert!(fx.blob_file(&d).exists());
    let mut got = Vec::new();
    reader.read_to_end(&mut got).unwrap();
    assert_eq!(got, bytes);
    drop(reader);
    assert_eq!(
        fx.svc.collect(&*fx.store).unwrap().unlinked,
        vec![d.clone()]
    );
    assert!(!fx.blob_file(&d).exists());
    assert_eq!(fx.blob(&d).unwrap().state, BlobState::Unlinked);
    assert!(matches!(
        fx.svc
            .open_stream(&authority, &*fx.store, &user, &stream_id),
        Err(StreamRefusal::NotFound)
    ));
    // The same bytes can be saved again later.
    fx.source("t3", "t3.bin", bytes, SourceOrigin::default());
    saved(fx.save(
        &user,
        "project-a",
        "Again",
        "application/octet-stream",
        "t3",
        bytes,
    ));
    assert_eq!(fx.blob(&d).unwrap().state, BlobState::Present);
    assert!(fx.blob_file(&d).exists());
}

#[test]
fn shared_references_outside_the_callers_scope_block_global_deletion() {
    let fx = Fx::new();
    let user = fx.user();
    let r = fx.saved("t1", "Shared", "text/plain", b"shared").artifact;
    let (svc, aid, ctx) = (fx.svc.clone(), r.artifact_id.clone(), user.clone());
    let shared = fx.write(move |tx| {
        as_failure(svc.share(
            tx,
            &ctx,
            &aid,
            &project("project-b"),
            &Revision::new(1).unwrap(),
        ))
    });
    assert!(shared.changed);
    let in_b = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    assert_eq!(read_text(&fx, &in_b, &r), "shared");
    // A principal confined to project-a may not delete what project-b still references.
    let in_a = fx.ctx("agent-p", PrincipalKind::Agent, Some("project-a"));
    let before = fx.artifact(&r.artifact_id);
    let f = refused(fx.delete(
        &in_a,
        delete_version(&r, before.revision, before.reference_revision),
    ));
    assert_eq!(
        (f.code, target(&f)),
        (DomainErrorCode::Conflict, SHARED_REFERENCES_REMAIN)
    );
    assert_eq!(fx.artifact(&r.artifact_id), before, "nothing changed");
    // Detaching changes only the named project's reference.
    let detach = |who: &InvocationContext, p: &str, rev: i64| {
        fx.delete(
            who,
            json!({"kind": "DetachProject", "artifactId": r.artifact_id, "projectId": p,
                "expectedReferenceRevision": rev.to_string()}),
        )
    };
    let f = refused(detach(&in_b, "project-a", before.reference_revision));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    assert!(summary(detach(&in_b, "project-b", before.reference_revision)).changed);
    assert_eq!(
        fault(fx.query(&in_b, "artifacts.read", version_query("Read", &r), None)).code,
        DomainErrorCode::NotFound
    );
    let after = fx.artifact(&r.artifact_id);
    assert!(!summary(detach(&user, "project-b", after.reference_revision)).changed);
    assert_eq!(
        refused(detach(&user, "project-a", after.reference_revision)).code,
        DomainErrorCode::Conflict
    );
    // With no reference outside its scope left, the confined principal may delete it.
    assert!(
        summary(fx.delete(
            &in_a,
            delete_version(&r, after.revision, after.reference_revision)
        ))
        .changed
    );
}

#[test]
fn storage_limits_refuse_visibly_and_never_delete() {
    let fx = Fx::new();
    let user = fx.user();
    let first = fx.saved("t1", "Small", "text/plain", &[b'a'; 10]).artifact;
    let schema = serde_json::to_value(
        descriptor("storage.artifactQuotaBytes")
            .unwrap()
            .schema_ref(),
    )
    .unwrap();
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": fx.key(), "body": {
        "kind": "settings.save_revision", "scope": {"kind": "Project", "projectId": "project-a"},
        "expectedRevision": "1",
        "entries": [{"key": "storage.artifactQuotaBytes", "value": {"schema": schema, "value": 15}}]}});
    assert!(matches!(
        recorded(fx.command(&user, "settings.save_revision", frame)).0,
        DomainDecision::Applied { .. }
    ));
    fx.source("t2", "t2.bin", &[b'b'; 10], SourceOrigin::default());
    let f = refused(fx.save(
        &user,
        "project-a",
        "Too big",
        "text/plain",
        "t2",
        &[b'b'; 10],
    ));
    assert_eq!(
        (f.code, target(&f)),
        (DomainErrorCode::CapacityWait, QUOTA_TARGET)
    );
    assert!(fx.blob(&sha256_hex(&[b'b'; 10])).is_none());
    // Nothing was deleted to make room, and the status is visible.
    let items = list(&fx, &user, "project-a");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].artifact.artifact_id, first.artifact_id);
    let status = fx.read(|s| as_failure(fx.svc.storage_status(s, &project("project-a"))));
    assert_eq!(
        (status.used_bytes, status.quota_bytes, status.at_limit),
        (10, 15, false)
    );
    // Bytes the project already holds add nothing; other projects inherit no limit.
    fx.source("t3", "t3.bin", &[b'a'; 10], SourceOrigin::default());
    saved(fx.save(
        &user,
        "project-a",
        "Same bytes",
        "text/plain",
        "t3",
        &[b'a'; 10],
    ));
    saved(fx.save(
        &user,
        "project-b",
        "Elsewhere",
        "text/plain",
        "t2",
        &[b'b'; 10],
    ));
}

#[test]
fn export_destinations_are_authorized_separately_and_preflighted() {
    let fx = Fx::new();
    let user = fx.user();
    let bytes = b"exported bytes";
    let r = fx.saved("t1", "Export me", "text/plain", bytes).artifact;
    let export = |ctx: &InvocationContext| match result(fx.query(
        ctx,
        "artifacts.export",
        version_query("Export", &r),
        None,
    )) {
        OperationResult::ArtifactsExport { value } => value,
        other => panic!("{other:?}"),
    };
    let e = export(&user);
    let out = fx.h.root.0.with_extension("out");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let resolver = NativeFilesystem::new();
    let authority = fx.authority();
    let app_data = fx.h.root.0.to_string_lossy().into_owned();
    let write = |ctx: &InvocationContext, stream: &Id, dest: &std::path::Path, overwrite: bool| {
        fx.svc.write_export(
            &authority,
            &*fx.store,
            &resolver,
            Some(&app_data),
            ctx,
            stream,
            dest.to_str().unwrap(),
            overwrite,
        )
    };
    let dest = out.join("copy.txt");
    write(&user, &e.stream_id, &dest, false).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(matches!(
        write(&user, &e.stream_id, &dest, false),
        Err(ExportRefusal::DestinationExists)
    ));
    std::fs::write(&dest, b"user edits").unwrap();
    write(&user, &e.stream_id, &dest, true).unwrap();
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        bytes,
        "overwrite only with explicit intent"
    );
    // An agent can read its own export stream but may not choose a filesystem destination.
    let agent = fx.ctx("agent-a", PrincipalKind::Agent, None);
    let mine = export(&agent);
    let mut stream = fx
        .svc
        .open_stream(&authority, &*fx.store, &agent, &mine.stream_id)
        .unwrap();
    let mut got = Vec::new();
    stream.read_to_end(&mut got).unwrap();
    assert_eq!(got, bytes);
    drop(stream);
    let elsewhere = out.join("agent.txt");
    assert!(matches!(
        write(&agent, &mine.stream_id, &elsewhere, false),
        Err(ExportRefusal::HumanOnly)
    ));
    assert!(matches!(
        write(&agent, &e.stream_id, &elsewhere, false),
        Err(ExportRefusal::Stream(StreamRefusal::NotFound))
    ));
    assert!(!elsewhere.exists());
    // The destination passes the destructive/target preflight.
    assert!(matches!(
        write(
            &user,
            &e.stream_id,
            &fx.h.root.0.join("exported.txt"),
            false
        ),
        Err(ExportRefusal::Blocked(BlockReason::ApplicationData))
    ));
    assert!(matches!(
        write(
            &user,
            &e.stream_id,
            &fx.h.root.0.join("workspace.sqlite"),
            true
        ),
        Err(ExportRefusal::Blocked(BlockReason::ApplicationData))
    ));
    assert!(matches!(
        write(&user, &e.stream_id, &out, true),
        Err(ExportRefusal::NotAFile)
    ));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&dest, out.join("link.txt")).unwrap();
        assert!(matches!(
            write(&user, &e.stream_id, &out.join("link.txt"), true),
            Err(ExportRefusal::Blocked(_))
        ));
    }
    let exported = fx
        .events()
        .into_iter()
        .filter(|ev| ev.kind == "ArtifactExported")
        .count();
    assert_eq!(exported, 2);
    let recorded_path = fx
        .events()
        .into_iter()
        .find(|ev| ev.kind == "ArtifactExported")
        .unwrap()
        .payload["destination"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(recorded_path.ends_with("copy.txt") && !recorded_path.contains('*'));
    assert_eq!(
        std::path::Path::new(&recorded_path),
        std::fs::canonicalize(&dest).unwrap()
    );
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn source_links_escapes_and_changed_content_are_refused_before_staging() {
    let fx = Fx::new();
    let user = fx.user();
    let refusal = |handle: &str, bytes: &[u8]| {
        let f = refused(fx.save(&user, "project-a", "Source", "text/plain", handle, bytes));
        (f.code, target(&f).to_owned())
    };
    let source = |c| (c, "artifact_source".to_owned());
    #[cfg(unix)]
    {
        let secret = fx.h.root.0.with_extension("secret");
        std::fs::write(&secret, b"private").unwrap();
        std::os::unix::fs::symlink(&secret, fx.sources.root.location.join("link.txt")).unwrap();
        fx.sources
            .handles
            .lock()
            .unwrap()
            .insert("link".into(), ("link.txt".into(), SourceOrigin::default()));
        assert_eq!(
            refusal("link", b"private"),
            source(DomainErrorCode::PermissionDenied)
        );
        let _ = std::fs::remove_file(secret);
    }
    fx.sources.handles.lock().unwrap().insert(
        "escape".into(),
        ("../outside.txt".into(), SourceOrigin::default()),
    );
    assert_eq!(
        refusal("escape", b"x"),
        source(DomainErrorCode::PermissionDenied)
    );
    assert_eq!(refusal("unknown", b"x"), source(DomainErrorCode::NotFound));
    assert_eq!(
        refusal("denied", b"x"),
        source(DomainErrorCode::PermissionDenied)
    );
    fx.source("changed", "changed.txt", b"actual", SourceOrigin::default());
    assert_eq!(
        refusal("changed", b"expected"),
        source(DomainErrorCode::SourceChanged)
    );
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": fx.key(), "command": {
        "kind": "Save", "projectId": "project-a", "title": "Huge", "mediaType": "text/plain",
        "source": {"kind": "Upload", "ticketId": "changed"}, "expectedDigest": sha256_hex(b"actual"),
        "expectedBytes": "100000000000"}});
    let f = refused(fx.command(&user, "artifacts.save", frame));
    assert_eq!(
        (f.code, target(&f)),
        (DomainErrorCode::InvalidArgument, "artifact_file_limit")
    );
    assert!(fx.staging_ops().is_empty());
    assert!(fx.files.stored_digests().unwrap().is_empty());
}

#[test]
fn retained_get_attachment_maps_to_artifacts_read() {
    let fx = Fx::new();
    let user = fx.user();
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend(2u32.to_be_bytes());
    png.extend(3u32.to_be_bytes());
    png.extend([8, 6, 0, 0, 0, 0xde, 0xad]);
    fx.source(
        "att-1",
        "imports/diagram.png",
        &png,
        SourceOrigin {
            original_name: Some("diagram.png".into()),
            ..SourceOrigin::default()
        },
    );
    let frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "Imported diagram",
        "image/png",
        json!({"kind": "Attachment", "attachmentId": "att-1"}),
        &png,
    );
    let r = saved(fx.command(&user, "artifacts.save", frame)).artifact;
    // The file comes back with its content type and original file name.
    let OperationResult::ArtifactsRead {
        value:
            ArtifactReadContent::Binary {
                artifact,
                stream_id,
                byte_length,
                ..
            },
    } = result(fx.query(&user, "artifacts.read", version_query("Read", &r), None))
    else {
        panic!()
    };
    assert_eq!(
        (artifact.media_type.as_str(), byte_length.get()),
        ("image/png", png.len() as i64)
    );
    let authority = fx.authority();
    let mut stream = fx
        .svc
        .open_stream(&authority, &*fx.store, &user, &stream_id)
        .unwrap();
    assert_eq!(stream.grant.suggested_name, "diagram.png");
    let mut got = Vec::new();
    stream.read_to_end(&mut got).unwrap();
    assert_eq!(got, png);
    drop(stream);
    // An image preview reports its dimensions without decoding pixels.
    let OperationResult::ArtifactsPreview { value: p } = result(fx.query(
        &user,
        "artifacts.preview",
        version_query("Preview", &r),
        None,
    )) else {
        panic!()
    };
    let doc: Value = serde_json::from_str(&read_text(&fx, &user, &p.preview)).unwrap();
    assert_eq!(
        (doc["width"].as_u64(), doc["height"].as_u64()),
        (Some(2), Some(3))
    );
    // An unknown attachment is not found.
    let unknown =
        json!({"kind": "Read", "artifactId": "artifact-none", "versionId": "version-none"});
    assert_eq!(
        fault(fx.query(&user, "artifacts.read", unknown, None)).code,
        DomainErrorCode::NotFound
    );
    // A stored file that cannot be read is also not found (the retained source behavior), and
    // recovery reports the missing file without changing metadata.
    std::fs::remove_file(fx.blob_file(&hex(&r.sha256))).unwrap();
    assert_eq!(
        fault(fx.query(&user, "artifacts.read", version_query("Read", &r), None)).code,
        DomainErrorCode::NotFound
    );
    let report = fx.svc.recover(&*fx.store).unwrap();
    assert_eq!(report.missing, vec![hex(&r.sha256)]);
    assert_eq!(fx.blob(&hex(&r.sha256)).unwrap().state, BlobState::Present);
}

#[test]
fn text_reads_page_by_byte_cursor_on_character_boundaries() {
    let fx = Fx::new();
    let user = fx.user();
    let text = "αβγ".repeat(30_000);
    let r = fx
        .saved("t1", "Long", "text/plain", text.as_bytes())
        .artifact;
    let mut cursor: Option<String> = None;
    let mut all = String::new();
    loop {
        match result(fx.query(
            &user,
            "artifacts.read",
            version_query("Read", &r),
            cursor.as_deref(),
        )) {
            OperationResult::ArtifactsRead {
                value:
                    ArtifactReadContent::Text {
                        content,
                        first_byte,
                        next_byte,
                        truncated,
                        ..
                    },
            } => {
                assert_eq!(first_byte.get() as usize, all.len());
                assert!(content.0.len() <= devworks_core::services::artifacts::READ_WINDOW);
                all.push_str(&content.0);
                assert_eq!(next_byte.get() as usize, all.len());
                if !truncated {
                    break;
                }
                cursor = Some(next_byte.get().to_string());
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(all, text);
    assert_eq!(
        fault(fx.query(
            &user,
            "artifacts.read",
            version_query("Read", &r),
            Some("x")
        ))
        .code,
        DomainErrorCode::InvalidArgument
    );
}

#[test]
fn the_artifact_migration_is_recorded_and_keeps_the_writer_floor() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 6)
        .expect("artifact migration");
    assert_eq!(
        m.checksum(),
        MIGRATION_0006_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (5, 5));
}

#[test]
fn a_cancelled_save_stages_nothing_and_unselected_variants_are_rejected() {
    let fx = Fx::new();
    let user = fx.user();
    fx.source("t1", "t1.bin", b"bytes", SourceOrigin::default());
    let cancellation = Cancellation::new();
    cancellation.cancel();
    let frame = fx.save_frame(
        "cancel-1",
        "project-a",
        "Cancelled",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t1"}),
        b"bytes",
    );
    let options = WriteOptions {
        cancellation: Some(cancellation),
        ..WriteOptions::default()
    };
    let reply = fx.command_with(&user, "artifacts.save", frame.clone(), options);
    assert_eq!(rejected(reply), Rejection::StorageUnavailable);
    assert!(fx.staging_ops().is_empty() && fx.files.staged().unwrap().is_empty());
    // No receipt was recorded, so the same command succeeds once it is not cancelled.
    saved(fx.command(&user, "artifacts.save", frame));
    // `Replace` and `ResolveCitation` are recorded wire variants no catalog operation selects:
    // replacement is the documents path and citation resolution the documents/retrieval path.
    let replace = json!({"protocol": "1.0-draft.1", "requestKey": "replace-1", "command": {
        "kind": "Replace", "artifactId": "a", "expectedRevision": "1",
        "source": {"kind": "Upload", "ticketId": "t1"}, "expectedDigest": sha256_hex(b"bytes"),
        "expectedBytes": "5"}});
    assert_eq!(
        rejected(fx.command(&user, "artifacts.save", replace)),
        Rejection::InvalidRequest
    );
    let unselected = json!({"protocol": "1.0-draft.1", "requestKey": "delete-1", "command": {
        "kind": "DeleteVersion", "artifactId": "a", "versionId": "v",
        "expectedRevision": "1", "expectedReferenceRevision": "1"}});
    assert_eq!(
        rejected(fx.command(&user, "artifacts.reference", unselected)),
        Rejection::InvalidRequest
    );
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let citation = json!({"protocol": "1.0-draft.1", "queryId": "q-c", "limit": 1, "query": {
        "kind": "ResolveCitation", "citation": {"source": {"kind": "Knowledge", "entryId": "e",
        "revision": "1"}, "blockId": "b", "contentDigest": sha256_hex(b"x"),
        "locator": {"kind": "Knowledge", "entryId": "e", "revision": "1"}}}});
    for op in ["artifacts.read", "artifacts.list"] {
        let reply = fx
            .cases
            .query(&gateway, &user, op, citation.to_string().as_bytes())
            .expect("addressable");
        assert_eq!(fault(reply).code, DomainErrorCode::InvalidArgument, "{op}");
    }
}

/// Saves through a cloned registry and store from inside another save's preparation, so it
/// commits between that save's quota pre-check and its unit of work.
fn concurrent_save(fx: &Fx, handle: &str, bytes: &[u8]) -> OnResolve {
    let (store, cases, ids, clock) = (
        fx.store.clone(),
        fx.cases.clone(),
        fx.h.ids.clone(),
        fx.h.clock.clone(),
    );
    let (user, frame) = (
        fx.user(),
        fx.save_frame(
            &fx.key(),
            "project-a",
            "Concurrent",
            "text/plain",
            json!({"kind": "Upload", "ticketId": handle}),
            bytes,
        ),
    );
    Box::new(move || {
        let authority = HostAuthority {
            store: &*store,
            clock: &*clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*store,
            ids: &*ids,
        };
        let reply = cases
            .command(
                &gateway,
                &user,
                "artifacts.save",
                frame.to_string().as_bytes(),
            )
            .unwrap();
        saved(reply);
    })
}

#[test]
fn a_save_refused_after_staging_resolves_its_staging_without_a_restart() {
    let fx = Fx::new();
    let user = fx.user();
    let schema = serde_json::to_value(
        descriptor("storage.artifactQuotaBytes")
            .unwrap()
            .schema_ref(),
    )
    .unwrap();
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": fx.key(), "body": {
        "kind": "settings.save_revision", "scope": {"kind": "Project", "projectId": "project-a"},
        "expectedRevision": "1",
        "entries": [{"key": "storage.artifactQuotaBytes", "value": {"schema": schema, "value": 15}}]}});
    assert!(matches!(
        recorded(fx.command(&user, "settings.save_revision", frame)).0,
        DomainDecision::Applied { .. }
    ));
    let (racy, filler) = ([b'r'; 10], [b'f'; 10]);
    fx.source("filler", "filler.bin", &filler, SourceOrigin::default());
    fx.source("racy", "racy.bin", &racy, SourceOrigin::default());
    // The quota pre-check passes (0 + 10 <= 15); a concurrent save then uses 10 bytes before
    // this save's unit of work, whose in-transaction check refuses it after staging.
    let hook = concurrent_save(&fx, "filler", &filler);
    fx.sources
        .on_resolve
        .lock()
        .unwrap()
        .insert("racy".into(), hook);
    let f = refused(fx.save(&user, "project-a", "Racy", "text/plain", "racy", &racy));
    assert_eq!(
        (f.code, target(&f)),
        (DomainErrorCode::CapacityWait, QUOTA_TARGET)
    );
    // Resolved now, not at the next boot: the staging operation failed under its fence and the
    // unreferenced file was collected; the concurrent artifact is untouched.
    let ops = fx.staging_ops_all();
    let racy_digest = sha256_hex(&racy);
    let mine: Vec<_> = ops
        .iter()
        .filter(|o| o.payload["digest"] == racy_digest)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].state, OperationState::Failed);
    let resolutions = mine[0]
        .latest_attempt()
        .unwrap()
        .observations
        .iter()
        .filter(|o| o.get("abandoned").is_some())
        .count();
    assert_eq!(resolutions, 1);
    assert!(!fx.blob_file(&racy_digest).exists());
    assert!(fx.files.staged().unwrap().is_empty());
    assert!(fx.blob_file(&sha256_hex(&filler)).exists());
    assert_eq!(list(&fx, &user, "project-a").len(), 1);
    // Nothing is resolved twice: recovery in this boot and after a restart finds nothing.
    assert_eq!(fx.svc.recover(&*fx.store).unwrap(), Default::default());
    let fx = fx.restart();
    let report = fx.svc.recover(&*fx.store).unwrap();
    assert_eq!(report, Default::default(), "{report:?}");
    let failed = mine[0].id.clone();
    let op = fx.read(|s| s.operations().operation(&failed)).unwrap();
    assert_eq!(op.revision, mine[0].revision, "not resolved again");
}

// ---------------------------------------------------------------------------------------------
// Review regressions: claims, concurrent recovery, preparation limits, integrity, sources.

/// The real file adapter with hooks at the interleaving points the review probed.
struct HookFiles {
    inner: Arc<NativeArtifactFiles>,
    before_publish: Mutex<Option<OnResolve>>,
    during_publish: Mutex<Option<OnResolve>>,
    after_publish: Mutex<Option<OnResolve>>,
    on_unlink: Mutex<Option<OnResolve>>,
    read_ranges: AtomicU64,
}
impl HookFiles {
    fn new(inner: Arc<NativeArtifactFiles>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            before_publish: Mutex::new(None),
            during_publish: Mutex::new(None),
            after_publish: Mutex::new(None),
            on_unlink: Mutex::new(None),
            read_ranges: AtomicU64::new(0),
        })
    }
    fn fire(slot: &Mutex<Option<OnResolve>>) {
        let hook = slot.lock().unwrap().take();
        if let Some(h) = hook {
            h();
        }
    }
}
/// A source that runs a hook after its first chunk (the staging file exists, partly written).
struct HookedReader<'a> {
    inner: &'a mut dyn Read,
    hook: Option<OnResolve>,
}
impl Read for HookedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let cap = buf.len().min(4);
        let n = self.inner.read(&mut buf[..cap])?;
        if let Some(h) = self.hook.take() {
            h();
        }
        Ok(n)
    }
}
impl ArtifactFiles for HookFiles {
    fn publish(
        &self,
        s: &str,
        source: &mut dyn Read,
        max: u64,
        d: &str,
        n: u64,
    ) -> Result<devworks_core::ports::files::PublishedBlob, FileFault> {
        Self::fire(&self.before_publish);
        let hook = self.during_publish.lock().unwrap().take();
        let mut reader = HookedReader {
            inner: source,
            hook,
        };
        let published = self.inner.publish(s, &mut reader, max, d, n);
        Self::fire(&self.after_publish);
        published
    }
    fn managed_path(&self, d: &str) -> Result<String, FileFault> {
        self.inner.managed_path(d)
    }
    fn present(&self, d: &str, n: u64) -> Result<bool, FileFault> {
        self.inner.present(d, n)
    }
    fn inspect(&self, d: &str) -> Result<Option<u64>, FileFault> {
        self.inner.inspect(d)
    }
    fn read_range(&self, d: &str, n: u64, o: u64, m: usize) -> Result<Vec<u8>, FileFault> {
        self.read_ranges.fetch_add(1, Ordering::SeqCst);
        self.inner.read_range(d, n, o, m)
    }
    fn verify(&self, d: &str, n: u64) -> Result<(), FileFault> {
        self.inner.verify(d, n)
    }
    fn open(&self, d: &str, n: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        self.inner.open(d, n)
    }
    fn unlink(&self, d: &str) -> Result<(), FileFault> {
        Self::fire(&self.on_unlink);
        self.inner.unlink(d)
    }
    fn quarantine(&self, d: &str) -> Result<(), FileFault> {
        self.inner.quarantine(d)
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.inner.staged()
    }
    fn remove_staged(&self, n: &str) -> Result<(), FileFault> {
        self.inner.remove_staged(n)
    }
    fn stored_digests(&self) -> Result<Vec<String>, FileFault> {
        self.inner.stored_digests()
    }
    fn write_derived(&self, k: &str, b: &[u8]) -> Result<(), FileFault> {
        self.inner.write_derived(k, b)
    }
    fn read_derived(&self, k: &str, m: usize) -> Result<Option<Vec<u8>>, FileFault> {
        self.inner.read_derived(k, m)
    }
    fn collect_derived(&self) -> Result<u64, FileFault> {
        self.inner.collect_derived()
    }
    fn export(&self, d: &str, n: u64, p: &std::path::Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, n, p, o)
    }
}

impl Fx {
    /// A second service over the same store and sources with other file/source ports.
    fn service_with(
        &self,
        files: Arc<dyn ArtifactFiles>,
        sources: Arc<dyn ArtifactSources>,
    ) -> (Arc<ArtifactService>, UseCases) {
        let svc = Arc::new(ArtifactService::new(
            files,
            sources,
            Arc::new(NativeFilesystem::new()),
            self.h.clock.clone(),
            self.h.ids.clone(),
        ));
        let cases = UseCases::new().with_artifacts(svc.clone());
        (svc, cases)
    }
    fn command_via(
        &self,
        cases: &UseCases,
        authority: &dyn AuthorityPort,
        ctx: &InvocationContext,
        frame: &Value,
        options: WriteOptions,
    ) -> DomainCommandReply {
        let gateway = Gateway {
            authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        cases
            .command_with(
                &gateway,
                ctx,
                "artifacts.save",
                frame.to_string().as_bytes(),
                options,
            )
            .unwrap()
    }
    /// Every Present blob still has its file (the invariant the review's race broke).
    fn assert_present_blobs_have_files(&self) {
        let blobs = self.read(|s| s.artifacts().blobs_in(BlobState::Present, None, 500));
        for b in blobs {
            assert!(
                self.blob_file(&b.digest).exists(),
                "Present blob {} has no file",
                b.digest
            );
        }
    }
}

/// A save through the shared registry whose reply lands in `slot` (no assertion inside).
fn save_into(
    fx: &Fx,
    handle: &str,
    bytes: &[u8],
    slot: Arc<Mutex<Option<DomainCommandReply>>>,
) -> OnResolve {
    let (store, cases, ids, clock) = (
        fx.store.clone(),
        fx.cases.clone(),
        fx.h.ids.clone(),
        fx.h.clock.clone(),
    );
    let (user, frame) = (
        fx.user(),
        fx.save_frame(
            &fx.key(),
            "project-a",
            "Concurrent",
            "text/plain",
            json!({"kind": "Upload", "ticketId": handle}),
            bytes,
        ),
    );
    Box::new(move || {
        let authority = HostAuthority {
            store: &*store,
            clock: &*clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*store,
            ids: &*ids,
        };
        let reply = cases
            .command(
                &gateway,
                &user,
                "artifacts.save",
                frame.to_string().as_bytes(),
            )
            .unwrap();
        *slot.lock().unwrap() = Some(reply);
    })
}

/// A save whose host lineage names work outside its project is refused by its unit of work
/// after its file was staged.
fn refused_after_staging(fx: &Fx) -> InvocationContext {
    fx.user()
        .with_lineage(None, Some(RootId(id("root-b"))), None)
}

#[test]
fn abandoning_a_staged_file_never_unlinks_bytes_a_concurrent_save_committed() {
    let fx = Fx::new();
    let bytes = b"same bytes for both saves";
    let digest = sha256_hex(bytes);
    fx.source("t1", "t1.bin", bytes, SourceOrigin::default());
    fx.source("t2", "t2.bin", bytes, SourceOrigin::default());
    let hook = HookFiles::new(fx.files.clone());
    let (_, cases) = fx.service_with(hook.clone(), fx.sources.clone());
    // Save B (same bytes) runs between A's resolution and its physical removal.
    let slot = Arc::new(Mutex::new(None));
    *hook.on_unlink.lock().unwrap() = Some(save_into(&fx, "t2", bytes, slot.clone()));
    let frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "A",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t1"}),
        bytes,
    );
    let authority = fx.authority();
    let a = refused_after_staging(&fx);
    let f = refused(fx.command_via(&cases, &authority, &a, &frame, WriteOptions::default()));
    assert_eq!(target(&f), "origin_work");
    // B could not commit metadata for a file A had claimed: it is refused as retryable.
    let b = slot
        .lock()
        .unwrap()
        .take()
        .expect("B ran during A's removal");
    assert_eq!(rejected(b), Rejection::StorageUnavailable);
    fx.assert_present_blobs_have_files();
    assert!(list(&fx, &fx.user(), "project-a").is_empty());
    // B's retry stages the bytes again and is readable.
    let r = saved(fx.save(&fx.user(), "project-a", "B", "text/plain", "t2", bytes)).artifact;
    assert_eq!(hex(&r.sha256), digest);
    assert_eq!(read_text(&fx, &fx.user(), &r).as_bytes(), bytes);
    fx.assert_present_blobs_have_files();
}

#[test]
fn a_digest_another_save_is_staging_is_never_claimed() {
    let fx = Fx::new();
    let bytes = b"staged twice";
    fx.source("t1", "t1.bin", bytes, SourceOrigin::default());
    fx.source("t2", "t2.bin", bytes, SourceOrigin::default());
    let hook = HookFiles::new(fx.files.clone());
    let (_, cases) = fx.service_with(hook.clone(), fx.sources.clone());
    // B is mid-staging (journaled, file published) while A is refused and resolved.
    let slot = Arc::new(Mutex::new(None));
    let a_frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "A",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t1"}),
        bytes,
    );
    let (store, cases_a, ids, clock, a) = (
        fx.store.clone(),
        cases.clone(),
        fx.h.ids.clone(),
        fx.h.clock.clone(),
        refused_after_staging(&fx),
    );
    let a_slot = slot.clone();
    *hook.after_publish.lock().unwrap() = Some(Box::new(move || {
        let authority = HostAuthority {
            store: &*store,
            clock: &*clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*store,
            ids: &*ids,
        };
        let reply = cases_a
            .command(
                &gateway,
                &a,
                "artifacts.save",
                a_frame.to_string().as_bytes(),
            )
            .unwrap();
        *a_slot.lock().unwrap() = Some(reply);
    }));
    let authority = fx.authority();
    let b_frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "B",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t2"}),
        bytes,
    );
    let r = saved(fx.command_via(
        &cases,
        &authority,
        &fx.user(),
        &b_frame,
        WriteOptions::default(),
    ))
    .artifact;
    assert_eq!(
        target(&refused(slot.lock().unwrap().take().unwrap())),
        "origin_work"
    );
    assert_eq!(read_text(&fx, &fx.user(), &r).as_bytes(), bytes);
    fx.assert_present_blobs_have_files();
}

#[test]
fn recovery_running_while_saves_stage_removes_nothing_in_flight() {
    let fx = Fx::new();
    let bytes = b"recover around me";
    fx.source("t1", "t1.bin", bytes, SourceOrigin::default());
    let hook = HookFiles::new(fx.files.clone());
    let (svc, cases) = fx.service_with(hook.clone(), fx.sources.clone());
    let reports = Arc::new(Mutex::new(Vec::new()));
    for slot in [
        &hook.before_publish,
        &hook.during_publish,
        &hook.after_publish,
    ] {
        let (svc, store, reports) = (fx.svc.clone(), fx.store.clone(), reports.clone());
        *slot.lock().unwrap() = Some(Box::new(move || {
            reports.lock().unwrap().push(svc.recover(&*store).unwrap());
        }));
    }
    let authority = fx.authority();
    let frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "Concurrent",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t1"}),
        bytes,
    );
    let r = saved(fx.command_via(
        &cases,
        &authority,
        &fx.user(),
        &frame,
        WriteOptions::default(),
    ))
    .artifact;
    let reports = reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 3);
    for report in &reports {
        assert_eq!(
            report,
            &RecoveryReport::default(),
            "touched in-flight staging"
        );
    }
    assert_eq!(read_text(&fx, &fx.user(), &r).as_bytes(), bytes);
    assert_eq!(svc.recover(&*fx.store).unwrap(), RecoveryReport::default());
}

#[test]
fn recovery_in_a_loop_alongside_many_saves_keeps_every_committed_file() {
    let fx = Fx::new();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (svc, store, flag) = (fx.svc.clone(), fx.store.clone(), stop.clone());
    let recoverer = std::thread::spawn(move || {
        let mut runs = 0;
        while !flag.load(Ordering::SeqCst) {
            svc.recover(&*store).unwrap();
            svc.collect(&*store).unwrap();
            runs += 1;
        }
        runs
    });
    let user = fx.user();
    let mut saved_refs = Vec::new();
    for i in 0..24 {
        // Half the saves share bytes with an earlier one.
        let bytes = format!("payload {}", i % 12).into_bytes();
        let handle = format!("h{i}");
        fx.source(
            &handle,
            &format!("{handle}.bin"),
            &bytes,
            SourceOrigin::default(),
        );
        saved_refs.push((
            saved(fx.save(&user, "project-a", "Loop", "text/plain", &handle, &bytes)).artifact,
            bytes,
        ));
    }
    stop.store(true, Ordering::SeqCst);
    assert!(recoverer.join().unwrap() > 0);
    for (r, bytes) in &saved_refs {
        assert_eq!(read_text(&fx, &user, r).as_bytes(), &bytes[..]);
    }
    fx.assert_present_blobs_have_files();
    assert!(fx.svc.recover(&*fx.store).unwrap().missing.is_empty());
}

#[test]
fn recovery_pages_through_every_unresolved_staging_operation() {
    let fx = Fx::new();
    let digest = sha256_hex(b"named by the last operation");
    // 600 operations of an earlier boot plus 1 of this boot naming a digest whose file exists.
    let journal = |n: usize, d: String| {
        fx.write(move |tx| {
            let mut last = None;
            for _ in 0..n {
                let id = devworks_core::protocol::OperationId(
                    tx.new_id(devworks_core::ports::IdKind::Operation),
                );
                tx.operations().record_intent(NewOperation {
                    id: id.clone(),
                    root_id: None,
                    assignment_id: None,
                    capability: STAGE_CAPABILITY.into(),
                    schema_version: 1,
                    args_digest: "x".into(),
                    payload: json!({"digest": d, "byteCount": 27}),
                })?;
                let attempt = tx
                    .operations()
                    .begin_attempt(&id, 1, AttemptBinding::default())?;
                tx.operations().transition(
                    &id,
                    2,
                    Some(&attempt.fence()),
                    OperationState::Dispatching,
                    None,
                )?;
                last = Some(id);
            }
            Ok(last)
        })
    };
    journal(600, sha256_hex(b"other"));
    let fx = fx.restart();
    // In flight in this boot: its digest file must survive recovery.
    let bytes = b"named by the last operation";
    fx.files
        .publish(
            "live.part",
            &mut &bytes[..],
            64,
            &digest,
            bytes.len() as u64,
        )
        .unwrap();
    journal_in(&fx, &digest);
    let report = fx.svc.recover(&*fx.store).unwrap();
    assert_eq!(report.failed.len(), 600);
    assert!(fx.blob_file(&digest).exists(), "{report:?}");
}
fn journal_in(fx: &Fx, d: &str) {
    let d = d.to_owned();
    fx.write(move |tx| {
        let id = devworks_core::protocol::OperationId(
            tx.new_id(devworks_core::ports::IdKind::Operation),
        );
        tx.operations().record_intent(NewOperation {
            id: id.clone(),
            root_id: None,
            assignment_id: None,
            capability: STAGE_CAPABILITY.into(),
            schema_version: 1,
            args_digest: "x".into(),
            payload: json!({"digest": d, "byteCount": 27}),
        })?;
        let attempt = tx
            .operations()
            .begin_attempt(&id, 1, AttemptBinding::default())?;
        tx.operations().transition(
            &id,
            2,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )?;
        Ok(())
    });
}

/// Authority that can only replay receipts.
struct ReplayOnly;
impl AuthorityPort for ReplayOnly {
    fn authorize(
        &self,
        _: &InvocationContext,
        _: &devworks_core::ports::AuthorityRequest<'_>,
    ) -> Result<devworks_core::ports::Authorization, devworks_core::ports::AuthorityDenial> {
        Ok(devworks_core::ports::Authorization::direct().replay_only())
    }
}

#[test]
fn replay_only_and_expired_commands_prepare_nothing() {
    let fx = Fx::new();
    let bytes = b"never read";
    fx.source("t1", "t1.bin", bytes, SourceOrigin::default());
    let resolved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = resolved.clone();
    fx.sources.on_resolve.lock().unwrap().insert(
        "t1".into(),
        Box::new(move || flag.store(true, Ordering::SeqCst)),
    );
    let frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "Replay",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t1"}),
        bytes,
    );
    let reply = fx.command_via(
        &fx.cases,
        &ReplayOnly,
        &fx.user(),
        &frame,
        WriteOptions::default(),
    );
    assert_eq!(rejected(reply), Rejection::PermissionDenied);
    assert!(!resolved.load(Ordering::SeqCst), "the source was resolved");
    assert!(fx.staging_ops_all().is_empty());
    // An expired command reads no source and stages nothing either.
    let options = WriteOptions {
        deadline_ms: Some(T0 - 1),
        ..WriteOptions::default()
    };
    let reply = fx.command_with(&fx.user(), "artifacts.save", frame.clone(), options);
    assert_eq!(rejected(reply), Rejection::StorageUnavailable);
    assert!(!resolved.load(Ordering::SeqCst), "the source was resolved");
    assert!(fx.staging_ops_all().is_empty());
    assert!(fx.files.staged().unwrap().is_empty());
    // A deadline that passes while the file streams stops it and resolves the staging.
    let hook = HookFiles::new(fx.files.clone());
    let (_, cases) = fx.service_with(hook.clone(), fx.sources.clone());
    let clock = fx.h.clock.clone();
    *hook.during_publish.lock().unwrap() = Some(Box::new(move || {
        clock.0.store(T0 + 10_000, Ordering::SeqCst);
    }));
    let options = WriteOptions {
        deadline_ms: Some(T0 + 5_000),
        ..WriteOptions::default()
    };
    let authority = fx.authority();
    let reply = fx.command_via(&cases, &authority, &fx.user(), &frame, options);
    assert_eq!(rejected(reply), Rejection::StorageUnavailable);
    let ops = fx.staging_ops_all();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].state, OperationState::Failed);
    assert!(fx.files.staged().unwrap().is_empty());
    assert!(!fx.blob_file(&sha256_hex(bytes)).exists());
}

/// A store that refuses to admit the next `n` writes (a resolution that cannot be written).
struct Flaky {
    inner: Arc<SqliteStore>,
    refuse: AtomicU64,
}
impl ApplicationStore for Flaky {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if self
            .refuse
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StorageError::Busy);
        }
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}

fn staged_for(fx: &Fx, handle: &str, bytes: &[u8]) -> StagedFile {
    fx.source(
        handle,
        &format!("{handle}.bin"),
        bytes,
        SourceOrigin::default(),
    );
    let env = Preparation::new(&*fx.store, None, None);
    let digest = Digest::try_from(sha256_hex(bytes)).unwrap();
    let source = ArtifactSource::Upload {
        ticket_id: id(handle),
    };
    match fx
        .svc
        .stage(
            &env,
            &fx.user(),
            &StageRequest {
                project: &project("project-a"),
                source: &source,
                expected_digest: &digest,
                expected_bytes: bytes.len() as i64,
                request_digest: "direct",
            },
        )
        .unwrap()
    {
        Staging::Staged(s) => s,
        other => panic!("{other:?}"),
    }
}
fn op_state(fx: &Fx, op: &devworks_core::protocol::OperationId) -> OperationState {
    let op = op.clone();
    fx.read(|s| s.operations().operation(&op)).unwrap().state
}

#[test]
fn unknown_commit_outcomes_and_unwritable_resolutions_resolve_within_the_boot() {
    let fx = Fx::new();
    let unknown = CommandReply::CommandStatusUnknown {
        protocol: devworks_core::protocol::Protocol::Draft1,
        request_key: devworks_core::protocol::RequestKey(id("k")),
        correlation_id: devworks_core::protocol::CorrelationId(id("c")),
    };
    // The unit of work never committed: the staging fails and its file is collected.
    let s = staged_for(&fx, "t1", b"lost acknowledgment");
    let env = Preparation::new(&*fx.store, None, None);
    let prepared = Prepared::new(Staging::Staged(s.clone()));
    fx.svc
        .settle(&fx.user(), Some(&prepared), Some(&unknown), &env);
    assert_eq!(op_state(&fx, &s.operation_id), OperationState::Failed);
    assert!(!fx.blob_file(&s.digest).exists());
    // The unit of work did commit (the acknowledgment was lost): the staging completes.
    let s = staged_for(&fx, "t2", b"committed after all");
    let st = s.clone();
    fx.write(move |tx| {
        tx.artifacts()
            .put_blob(&st.digest, st.byte_count, &st.relative_path)?;
        let artifact = StoredArtifact {
            id: id("artifact-x"),
            origin_project_id: project("project-a"),
            origin_work_id: None,
            title: "X".into(),
            revision: 1,
            current_project_id: project("project-a"),
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        };
        tx.artifacts().insert_artifact(&artifact)?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("version-x"),
            artifact_id: id("artifact-x"),
            version_no: 1,
            blob_digest: st.digest.clone(),
            byte_count: st.byte_count,
            media_type: "text/plain".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({"operationId": st.operation_id.0.as_str()}),
        })
    });
    let prepared = Prepared::new(Staging::Staged(s.clone()));
    fx.svc
        .settle(&fx.user(), Some(&prepared), Some(&unknown), &env);
    assert_eq!(
        op_state(&fx, &s.operation_id),
        OperationState::ObservedComplete
    );
    assert!(fx.blob_file(&s.digest).exists());
    // A resolution that cannot be written is retried by the next collection in this boot.
    let s = staged_for(&fx, "t3", b"resolution deferred");
    let flaky = Flaky {
        inner: fx.store.clone(),
        refuse: AtomicU64::new(1),
    };
    let env = Preparation::new(&flaky, None, None);
    let prepared = Prepared::new(Staging::Staged(s.clone()));
    fx.svc.settle(&fx.user(), Some(&prepared), None, &env);
    assert_eq!(op_state(&fx, &s.operation_id), OperationState::Dispatching);
    fx.svc.collect(&*fx.store).unwrap();
    assert_eq!(op_state(&fx, &s.operation_id), OperationState::Failed);
    assert!(!fx.blob_file(&s.digest).exists());
}

#[test]
fn a_corrupt_existing_file_is_an_integrity_refusal_and_recovery_quarantines_it() {
    let fx = Fx::new();
    let user = fx.user();
    let bytes = b"0123456789 evidence";
    let first = fx.saved("t1", "First", "text/plain", bytes).artifact;
    let d = hex(&first.sha256);
    // Same length, different content.
    std::fs::write(fx.blob_file(&d), b"9876543210 evidence").unwrap();
    // Corrupt bytes are never served.
    assert_eq!(
        fault(fx.query(&user, "artifacts.read", version_query("Read", &first), None)).code,
        DomainErrorCode::NotFound
    );
    fx.source("t2", "t2.bin", bytes, SourceOrigin::default());
    let f = refused(fx.save(&user, "project-a", "Again", "text/plain", "t2", bytes));
    assert_eq!(
        (f.code, target(&f), f.retry),
        (
            DomainErrorCode::StorageUnavailable,
            INTEGRITY_TARGET,
            devworks_core::protocol::RetryAdvice::Never
        )
    );
    let report = fx.svc.recover(&*fx.store).unwrap();
    assert_eq!(report.quarantined, vec![d.clone()]);
    assert!(!fx.blob_file(&d).exists());
    assert_eq!(
        fx.blob(&d).unwrap().state,
        BlobState::Present,
        "metadata unchanged"
    );
    // Saving the bytes again publishes a verified copy; both versions read again.
    fx.source("t3", "t3.bin", bytes, SourceOrigin::default());
    let again = saved(fx.save(&user, "project-a", "Third", "text/plain", "t3", bytes)).artifact;
    assert_eq!(read_text(&fx, &user, &again).as_bytes(), bytes);
    assert_eq!(read_text(&fx, &user, &first).as_bytes(), bytes);
}

#[test]
fn a_stream_whose_file_disappeared_is_not_found() {
    let fx = Fx::new();
    let user = fx.user();
    let r = fx
        .saved("t1", "Binary", "application/octet-stream", b"\x00\x01\x02")
        .artifact;
    let OperationResult::ArtifactsRead {
        value: ArtifactReadContent::Binary { stream_id, .. },
    } = result(fx.query(&user, "artifacts.read", version_query("Read", &r), None))
    else {
        panic!()
    };
    std::fs::remove_file(fx.blob_file(&hex(&r.sha256))).unwrap();
    let authority = fx.authority();
    assert!(matches!(
        fx.svc
            .open_stream(&authority, &*fx.store, &user, &stream_id),
        Err(StreamRefusal::NotFound)
    ));
}

#[test]
fn history_aggregates_never_repeat_a_revision() {
    let fx = Fx::new();
    let user = fx.user();
    let r = fx.saved("t1", "Agg", "text/plain", b"aggregates").artifact;
    summary(fx.reference(&user, &r, json!({"kind": "root", "id": "root-a"}), 1));
    let (svc, aid, ctx) = (fx.svc.clone(), r.artifact_id.clone(), user.clone());
    fx.write(move |tx| {
        as_failure(svc.share(
            tx,
            &ctx,
            &aid,
            &project("project-b"),
            &Revision::new(1).unwrap(),
        ))
    });
    let a = fx.artifact(&r.artifact_id);
    summary(fx.delete(
        &user,
        json!({"kind": "DetachProject", "artifactId": r.artifact_id, "projectId": "project-b",
            "expectedReferenceRevision": a.reference_revision.to_string()}),
    ));
    let a = fx.artifact(&r.artifact_id);
    summary(fx.reference(
        &user,
        &r,
        json!({"kind": "root", "id": "task-a"}),
        a.reference_revision,
    ));
    let mut seen = std::collections::HashSet::new();
    for e in fx.events() {
        if e.aggregate_id == r.artifact_id.as_str() {
            assert!(
                seen.insert((e.aggregate_kind.clone(), e.aggregate_revision)),
                "{} repeats {} revision {}",
                e.kind,
                e.aggregate_kind,
                e.aggregate_revision
            );
        }
    }
    assert!(seen.len() >= 5);
}

#[test]
fn previews_are_served_from_the_derived_cache_until_it_is_collected() {
    let fx = Fx::new();
    let user = fx.user();
    let hook = HookFiles::new(fx.files.clone());
    let (svc, cases) = fx.service_with(hook.clone(), fx.sources.clone());
    fx.source("t1", "t1.bin", b"cached preview", SourceOrigin::default());
    let frame = fx.save_frame(
        &fx.key(),
        "project-a",
        "P",
        "text/plain",
        json!({"kind": "Upload", "ticketId": "t1"}),
        b"cached preview",
    );
    let authority = fx.authority();
    let r =
        saved(fx.command_via(&cases, &authority, &user, &frame, WriteOptions::default())).artifact;
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let preview = || {
        let frame = json!({"protocol": "1.0-draft.1", "queryId": "q-p", "limit": 1,
            "query": version_query("Preview", &r)});
        match result(
            cases
                .query(
                    &gateway,
                    &user,
                    "artifacts.preview",
                    frame.to_string().as_bytes(),
                )
                .unwrap(),
        ) {
            OperationResult::ArtifactsPreview { value } => value.preview,
            other => panic!("{other:?}"),
        }
    };
    let first = preview();
    let reads = hook.read_ranges.load(Ordering::SeqCst);
    assert!(reads >= 1);
    let second = preview();
    assert_eq!(
        hook.read_ranges.load(Ordering::SeqCst),
        reads,
        "cache hit reads the source"
    );
    assert_eq!(hex(&first.sha256), hex(&second.sha256));
    assert!(svc.collect_derived().unwrap() >= 1);
    let third = preview();
    assert!(
        hook.read_ranges.load(Ordering::SeqCst) > reads,
        "rebuilt after collection"
    );
    assert_eq!(hex(&first.sha256), hex(&third.sha256));
}

#[test]
fn host_issued_sources_resolve_only_for_their_binding() {
    use crate::fs::sources::{HostArtifactSources, IssueRefusal};
    let fx = Fx::new();
    let sources = Arc::new(HostArtifactSources::new(
        fx.h.clock.clone(),
        fx.h.ids.clone(),
    ));
    let (_, cases) = fx.service_with(fx.files.clone(), sources.clone());
    let authority = fx.authority();
    let dir = fx.h.root.0.with_extension("picked");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("picked.txt");
    std::fs::write(&file, b"picked by the user").unwrap();
    let bytes = b"picked by the user";
    let save = |ctx: &InvocationContext, p: &str, source: Value| {
        let frame = fx.save_frame(&fx.key(), p, "Picked", "text/plain", source, bytes);
        fx.command_via(&cases, &authority, ctx, &frame, WriteOptions::default())
    };
    let in_a = fx
        .user()
        .with_lineage(Some(project("project-a")), None, None);
    // Only user ingress mints upload tickets, and only for regular files.
    assert_eq!(
        sources.mint_upload(
            &fx.ctx("agent-a", PrincipalKind::Agent, None),
            &file,
            60_000
        ),
        Err(IssueRefusal::HumanOnly)
    );
    assert_eq!(
        sources.mint_upload(&in_a, &dir, 60_000),
        Err(IssueRefusal::NotAFile)
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&file, dir.join("link.txt")).unwrap();
        assert_eq!(
            sources.mint_upload(&in_a, &dir.join("link.txt"), 60_000),
            Err(IssueRefusal::NotAFile)
        );
    }
    let ticket = sources.mint_upload(&in_a, &file, 60_000).unwrap();
    let upload = json!({"kind": "Upload", "ticketId": ticket});
    // Another principal, another project and an expired ticket are refused.
    let agent = fx.ctx("agent-a", PrincipalKind::Agent, Some("project-a"));
    assert_eq!(
        refused(save(&agent, "project-a", upload.clone())).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        refused(save(&fx.user(), "project-b", upload.clone())).code,
        DomainErrorCode::PermissionDenied
    );
    let r = saved(save(&in_a, "project-a", upload.clone())).artifact;
    assert_eq!(read_text(&fx, &in_a, &r).as_bytes(), bytes);
    let stored = fx.artifact(&r.artifact_id);
    assert_eq!(stored.provenance["originalName"], "picked.txt");
    fx.h.clock.0.store(T0 + 61_000, Ordering::SeqCst);
    assert_eq!(
        refused(save(&in_a, "project-a", upload)).code,
        DomainErrorCode::NotFound
    );
    fx.h.clock.0.store(T0, Ordering::SeqCst);
    // Attachments and work outputs are registered by the host for their owning records.
    let host = fx.ctx("host", PrincipalKind::Host, None);
    assert_eq!(
        sources.register_attachment(
            &fx.user(),
            &id("att-1"),
            &project("project-a"),
            &dir,
            "picked.txt",
            None
        ),
        Err(IssueRefusal::HostOnly)
    );
    sources
        .register_attachment(
            &host,
            &id("att-1"),
            &project("project-a"),
            &dir,
            "picked.txt",
            Some("orig.txt".into()),
        )
        .unwrap();
    let attachment = json!({"kind": "Attachment", "attachmentId": "att-1"});
    assert_eq!(
        refused(save(&fx.user(), "project-b", attachment.clone())).code,
        DomainErrorCode::PermissionDenied
    );
    saved(save(&fx.user(), "project-a", attachment.clone()));
    // A handle of another kind is unknown.
    assert_eq!(
        refused(save(
            &fx.user(),
            "project-a",
            json!({"kind": "WorkOutput", "fileHandle": "att-1"})
        ))
        .code,
        DomainErrorCode::NotFound
    );
    sources
        .register_work_output(
            &host,
            &id("out-1"),
            &actor("agent-a"),
            &project("project-a"),
            Some(RootId(id("root-a"))),
            &dir,
            "picked.txt",
            SourceOrigin {
                repository_revision: Some("abc123".into()),
                ..SourceOrigin::default()
            },
        )
        .unwrap();
    let output = json!({"kind": "WorkOutput", "fileHandle": "out-1"});
    let worker = fx.worker("agent-a");
    let r = saved(save(&worker, "project-a", output.clone())).artifact;
    assert_eq!(
        fx.artifact(&r.artifact_id).provenance["repositoryRevision"],
        "abc123"
    );
    assert_eq!(
        refused(save(&fx.worker("agent-p"), "project-a", output)).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        refused(save(
            &fx.user(),
            "project-a",
            json!({"kind": "Upload", "ticketId": "unknown"})
        ))
        .code,
        DomainErrorCode::NotFound
    );
    let _ = std::fs::remove_dir_all(&dir);
}
