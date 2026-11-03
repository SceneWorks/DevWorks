//! The boot entry point on the real SQLite store and real managed files: a process that dies
//! after a receipt commits (before its reply is claimed, or while the claimed reply's file is
//! written but its metadata is not) is reopened on the same data root, booted through
//! `devworks_engine::boot`, and its reply is published exactly once; required consumers cannot
//! be displaced; an applied deletion starts a collection pass. No inference, network or sleeps
//! used as synchronization.
mod common;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::authority::destructive::FileIdentity;
use devworks_core::authority::reads::ReadRoot;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::files::{
    ArtifactFiles, ArtifactSources, FileFault, PublishedBlob, ResolvedSource, SourceOrigin,
    SourceRefusal,
};
use devworks_core::ports::storage::{
    ApplicationStore, BlobState, ReadOptions, ReadSnapshot, StorageError, StoreInfo, Unavailable,
    WriteJob, WriteOptions,
};
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::records::{ReceiptLookupState, ReceiptObservation};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{Check, CommandReply, ProjectId};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::replies::REPLY_CONSUMER;
use devworks_core::services::history::HistoryService;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::sessions::SessionService;
use devworks_engine::{BootError, BootReport, Composition, Runtime, RuntimeConfig, boot};
use serde_json::json;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// A store whose process can die: once `dead`, nothing more reaches the database (every write
/// and read is refused), exactly as if the process had stopped at that instant.
struct Mortal {
    inner: Arc<dyn ApplicationStore>,
    dead: Arc<AtomicBool>,
}
impl Mortal {
    fn gone(&self) -> Result<(), StorageError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(StorageError::unavailable(
                Unavailable::WriterStopped,
                "the process died",
            ));
        }
        Ok(())
    }
}
impl ApplicationStore for Mortal {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.gone()?;
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.gone()?;
        self.inner.read(options, work)
    }
}

/// Where the first boot's process dies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    /// Right after the command's receipt (and its reply intent) committed, before any claim.
    AfterReceipt,
    /// While the reply delivery is claimed: the reply file is in place, its metadata is not.
    AfterReplyFile,
}

/// The native adapter; when `die` is set, the process dies right after a publication renamed
/// its file into place.
struct Files {
    inner: NativeArtifactFiles,
    die: Option<(Arc<AtomicBool>, Arc<Notify>)>,
}
impl ArtifactFiles for Files {
    fn publish(
        &self,
        s: &str,
        src: &mut dyn Read,
        d: &str,
        n: u64,
        r: Option<&str>,
        g: &str,
    ) -> Result<PublishedBlob, FileFault> {
        let published = self.inner.publish(s, src, d, n, r, g);
        if let Some((dead, died)) = &self.die {
            dead.store(true, Ordering::SeqCst);
            died.notify_one();
        }
        published
    }
    fn managed_path(&self, d: &str, g: &str) -> Result<String, FileFault> {
        self.inner.managed_path(d, g)
    }
    fn present(&self, d: &str, g: &str, n: u64) -> Result<bool, FileFault> {
        self.inner.present(d, g, n)
    }
    fn inspect(&self, d: &str, g: &str) -> Result<Option<u64>, FileFault> {
        self.inner.inspect(d, g)
    }
    fn read_range(&self, d: &str, g: &str, n: u64, o: u64, m: usize) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range(d, g, n, o, m)
    }
    fn open(&self, d: &str, g: &str, n: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        self.inner.open(d, g, n)
    }
    fn unlink(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.unlink(d, g)
    }
    fn quarantine(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.quarantine(d, g)
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.inner.staged()
    }
    fn remove_staged(&self, n: &str) -> Result<(), FileFault> {
        self.inner.remove_staged(n)
    }
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        self.inner.stored_files()
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
    fn export(&self, d: &str, g: &str, n: u64, dest: &Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, g, n, dest, o)
    }
}

/// Upload tickets naming files beneath one registered read root.
struct Sources {
    root: ReadRoot,
}
impl ArtifactSources for Sources {
    fn resolve(
        &self,
        _: &InvocationContext,
        source: &ArtifactSource,
        _: Option<&ProjectId>,
    ) -> Result<ResolvedSource, SourceRefusal> {
        match source {
            ArtifactSource::Upload { ticket_id } => Ok(ResolvedSource {
                root: self.root.clone(),
                relative: format!("{}.bin", ticket_id.as_str()),
                origin: SourceOrigin::default(),
            }),
            _ => Err(SourceRefusal::NotFound),
        }
    }
}
fn sources(h: &Harness) -> Arc<Sources> {
    let location = h.root.with_extension("sources");
    let _ = std::fs::remove_dir_all(&location);
    std::fs::create_dir_all(&location).unwrap();
    let identity = identity(&std::fs::metadata(&location).unwrap());
    Arc::new(Sources {
        root: ReadRoot { location, identity },
    })
}
#[cfg(unix)]
fn identity(m: &std::fs::Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        volume: m.dev(),
        file: m.ino(),
    })
}
#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> Option<FileIdentity> {
    None
}

fn native(h: &Harness) -> NativeArtifactFiles {
    NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache"))
}

/// The production composition over `store`, with one artifact service.
fn composition(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    files: Arc<dyn ArtifactFiles>,
    sources: Arc<dyn ArtifactSources>,
) -> Composition {
    let artifacts = Arc::new(ArtifactService::new(
        files,
        sources,
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    // Only the boot pass and triggered passes run within a test.
    config.collect_interval = Duration::from_secs(3600);
    Composition {
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(Authority),
        artifacts,
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: Arc::new(NativeProcessHost),
        runs: devworks_engine::RunComposition::new(Arc::new(
            devworks_core::services::runs::ControlService::new(h.ids.clone()),
        )),
        power: Arc::new(NativePower),
        use_cases: UseCases::new()
            .with_queue()
            .with_sessions(Arc::new(SessionService::new(h.ids.clone(), store)))
            .with_history(Arc::new(HistoryService::new(h.ids.clone()))),
        consumers: Vec::new(),
        config,
    }
}

fn start(r: &Runtime, ctx: &InvocationContext, key: &str) -> DomainCommandReply {
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
        "body": {"kind": "sessions.start", "scope": {"kind": "Workspace"}, "title": "Home"}});
    r.command(ctx, "sessions.start", frame.to_string().as_bytes())
        .expect("addressable")
}
fn observation(r: &Runtime, ctx: &InvocationContext, key: &str) -> ReceiptObservation {
    let frame = json!({"protocol": "1.0-draft.1", "queryId": "q-r",
        "query": {"kind": "history.command_receipt", "requestKey": key}});
    match r
        .query(ctx, "history.command_receipt", frame.to_string().as_bytes())
        .expect("addressable")
    {
        DomainQueryReply::Result {
            result: OperationResult::HistoryCommandReceipt { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    }
}

/// Counts rows in the store file (read-only, beside the open store).
fn count(h: &Harness, sql: &str) -> i64 {
    let c = rusqlite::Connection::open_with_flags(
        h.root.join("workspace.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The durable effects of exactly one published reply, and nothing pending.
fn assert_published_once(h: &Harness, store: &dyn ApplicationStore) {
    let status = read(store, |s| s.queue().status(REPLY_CONSUMER));
    assert_eq!(
        (
            status.completed,
            status.ready,
            status.claimed,
            status.reconciling,
            status.failed,
            status.cancelled,
            status.dismissed
        ),
        (1, 0, 0, 0, 0, 0, 0),
        "{status:?}"
    );
    assert_eq!(count(h, "SELECT count(*) FROM receipt_replies"), 1);
    assert_eq!(count(h, "SELECT count(*) FROM artifacts"), 1);
    assert_eq!(
        count(
            h,
            "SELECT count(*) FROM events WHERE kind='ReceiptReplyPublished'"
        ),
        1
    );
    assert_eq!(native(h).stored_files().unwrap().len(), 1, "no orphan");
    assert_eq!(
        count(
            h,
            "SELECT count(*) FROM operations WHERE state NOT IN \
             ('ObservedComplete','Failed','CancelledBeforeDispatch')"
        ),
        0,
        "every staging operation is resolved"
    );
}

/// Drives delivery until the reply consumer has completed `n` deliveries.
async fn delivered(r: &Runtime, n: i64) {
    loop {
        let done = read(&**r.store(), |s| s.queue().status(REPLY_CONSUMER)).completed;
        if done >= n {
            return;
        }
        r.dispatcher().run_until_idle().await.unwrap();
    }
}

fn recovered_nothing(report: &BootReport) -> bool {
    let (q, a) = (&report.queue, &report.artifacts);
    q.classified.is_empty()
        && q.requeued.is_empty()
        && q.held.is_empty()
        && q.cancelled.is_empty()
        && a.completed.is_empty()
        && a.failed.is_empty()
        && a.removed_staging.is_empty()
        && a.removed_orphans.is_empty()
}

#[test]
fn a_reply_interrupted_by_a_crash_is_delivered_exactly_once_after_boot() {
    for crash in [Crash::AfterReceipt, Crash::AfterReplyFile] {
        let h = Harness::new();
        let src = sources(&h);
        // First boot: the process dies at `crash`.
        let (receipt, user_name) = {
            let rt = runtime();
            let dead = Arc::new(AtomicBool::new(false));
            let died = Arc::new(Notify::new());
            let store: Arc<dyn ApplicationStore> = Arc::new(Mortal {
                inner: h.open(),
                dead: dead.clone(),
            });
            let files = Arc::new(Files {
                inner: native(&h),
                die: (crash == Crash::AfterReplyFile).then(|| (dead.clone(), died.clone())),
            });
            let user = principal(&*store, "user-a", PrincipalKind::Human);
            let app = rt
                .block_on(boot(composition(&h, store.clone(), files, src.clone())))
                .unwrap();
            assert!(recovered_nothing(app.report()));
            let reply = start(&app, &user, "start-1");
            let CommandReply::Recorded { receipt, .. } = reply else {
                panic!("{reply:?}")
            };
            match crash {
                Crash::AfterReceipt => dead.store(true, Ordering::SeqCst),
                // Delivery claims the reply and writes its file; the process dies before the
                // metadata, the staging resolution or the delivery's outcome is recorded.
                Crash::AfterReplyFile => {
                    rt.block_on(within("the crash", died.notified()));
                }
            }
            drop(app);
            drop(rt);
            drop(store);
            (receipt, user.principal().0.as_str().to_owned())
        };
        let after_crash_files = native(&h).stored_files().unwrap().len();
        assert_eq!(
            (
                count(&h, "SELECT count(*) FROM artifacts"),
                after_crash_files
            ),
            match crash {
                Crash::AfterReceipt => (0, 0),
                Crash::AfterReplyFile => (0, 1),
            },
            "{crash:?}: nothing visible, at most an orphaned file"
        );

        // Second boot on the same data root, through the entry point.
        let rt = runtime();
        let store = h.open();
        let user = principal(&*store, &user_name, PrincipalKind::Human);
        let app = rt
            .block_on(boot(composition(
                &h,
                store.clone(),
                Arc::new(native(&h)),
                src.clone(),
            )))
            .unwrap();
        let report = app.report().clone();
        match crash {
            Crash::AfterReceipt => assert!(recovered_nothing(&report), "{report:?}"),
            Crash::AfterReplyFile => {
                // The dead process's claim is requeued; its staging operation is classified,
                // then failed (its metadata never committed) and the orphaned file removed.
                assert_eq!(report.queue.requeued.len(), 1, "{report:?}");
                assert!(report.queue.held.is_empty(), "{report:?}");
                assert_eq!(report.queue.classified.len(), 1, "{report:?}");
                assert_eq!(report.artifacts.failed.len(), 1, "{report:?}");
                assert_eq!(report.artifacts.removed_orphans.len(), 1, "{report:?}");
                assert_eq!(
                    report.queue.classified[0].0, report.artifacts.failed[0],
                    "{report:?}"
                );
            }
        }
        rt.block_on(within("publication after restart", delivered(&app, 1)));
        assert_published_once(&h, &*store);
        let o = observation(&app, &user, "start-1");
        o.check().unwrap();
        assert!(matches!(o.state, ReceiptLookupState::Recorded), "{o:?}");
        assert_eq!(o.command_id.as_ref(), Some(&receipt.command_id));
        assert!(o.reply_artifact.is_some() && o.reply_schema.is_some());
        rt.block_on(app.shutdown()).into_result().unwrap();
        drop(app);
        drop(rt);
        drop(store);

        // A further boot finds nothing to recover and publishes nothing again.
        let rt = runtime();
        let store = h.open();
        let app = rt
            .block_on(boot(composition(
                &h,
                store.clone(),
                Arc::new(native(&h)),
                src.clone(),
            )))
            .unwrap();
        assert!(recovered_nothing(app.report()), "{crash:?}");
        let claimed = rt.block_on(within("idle", app.dispatcher().run_until_idle()));
        assert_eq!(claimed.unwrap(), 0, "{crash:?}");
        assert_published_once(&h, &*store);
        rt.block_on(app.shutdown()).into_result().unwrap();
    }
}

#[test]
fn boot_registers_the_reply_consumer_and_a_caller_cannot_displace_it() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    let mut c = composition(&h, store.clone(), Arc::new(native(&h)), sources(&h));
    c.consumers.push(consumer(REPLY_CONSUMER, Recorder::ok()));
    let refused = rt.block_on(boot(c));
    assert!(
        matches!(refused, Err(BootError::Composition(_))),
        "{:?}",
        refused.err()
    );
    // Nothing was started: the composition is refused before recovery.
    let user = principal(&*store, "user-a", PrincipalKind::Human);
    let app = rt
        .block_on(boot(composition(
            &h,
            store.clone(),
            Arc::new(native(&h)),
            sources(&h),
        )))
        .unwrap();
    assert!(matches!(
        start(&app, &user, "start-1"),
        CommandReply::Recorded { .. }
    ));
    // The command woke delivery: the running dispatcher publishes the reply without any other
    // prompt.
    rt.block_on(within("published by the running dispatcher", async {
        let mut changes = app.dispatcher().subscribe();
        while read(&*store, |s| s.queue().status(REPLY_CONSUMER)).completed < 1 {
            changes.changed().await.unwrap();
        }
    }));
    observation(&app, &user, "start-1").check().unwrap();
    rt.block_on(app.shutdown()).into_result().unwrap();
}

fn applied(reply: DomainCommandReply) -> OperationResult {
    match reply {
        CommandReply::Recorded {
            receipt:
                devworks_core::protocol::Receipt {
                    decision: DomainDecision::Applied { result },
                    ..
                },
            ..
        } => result,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_applied_delete_starts_a_collection_pass_that_removes_the_released_file() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    seed_root(&h, "root-a");
    let src = sources(&h);
    let user = principal(&*store, "user-a", PrincipalKind::Human);
    let app = rt
        .block_on(boot(composition(
            &h,
            store.clone(),
            Arc::new(native(&h)),
            src.clone(),
        )))
        .unwrap();
    let mut passes = app.collections();
    rt.block_on(within("the boot collection pass", async {
        while passes.borrow_and_update().is_none() {
            passes.changed().await.unwrap();
        }
    }));
    let boot_pass = passes.borrow().clone().unwrap();
    assert_eq!(boot_pass.pass, 1);
    assert_eq!(boot_pass.blobs.unwrap(), Default::default());

    let bytes = b"evidence that will be deleted";
    let digest = sha256_hex(bytes);
    std::fs::write(src.root.location.join("t1.bin"), bytes).unwrap();
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": "save-1", "command": {
        "kind": "Save", "projectId": "project-a", "title": "Evidence",
        "mediaType": "application/octet-stream", "source": {"kind": "Upload", "ticketId": "t1"},
        "expectedDigest": digest, "expectedBytes": bytes.len().to_string()}});
    let OperationResult::ArtifactsSave { value: saved } = applied(
        app.command(&user, "artifacts.save", frame.to_string().as_bytes())
            .unwrap(),
    ) else {
        panic!()
    };
    let blob_file = |digest: &str| {
        native(&h)
            .stored_files()
            .unwrap()
            .into_iter()
            .any(|(d, _)| d == digest)
    };
    assert!(blob_file(&digest));
    let aid = saved.artifact.artifact_id.clone();
    let stored = read(&*store, move |s| s.artifacts().artifact(&aid)).unwrap();
    let deletion = json!({"protocol": "1.0-draft.1", "requestKey": "delete-1", "deletion": {
        "kind": "DeleteVersion", "artifactId": saved.artifact.artifact_id,
        "versionId": saved.artifact.version_id,
        "expectedRevision": stored.revision.to_string(),
        "expectedReferenceRevision": stored.reference_revision.to_string()}});
    assert!(matches!(
        applied(
            app.command(&user, "artifacts.delete", deletion.to_string().as_bytes())
                .unwrap()
        ),
        OperationResult::ArtifactsDelete { .. }
    ));
    // The periodic interval is an hour: only the deletion's trigger can collect it now.
    let d = digest.clone();
    let pass = rt.block_on(within("collection after the deletion", async {
        loop {
            passes.changed().await.unwrap();
            let pass = passes.borrow_and_update().clone().unwrap();
            if pass.blobs.as_ref().unwrap().unlinked.contains(&d) {
                return pass;
            }
        }
    }));
    assert!(pass.pass > boot_pass.pass);
    assert!(!blob_file(&digest));
    let d = digest.clone();
    assert_eq!(
        read(&*store, move |s| s.artifacts().blob(&d))
            .unwrap()
            .state,
        BlobState::Unlinked
    );
    rt.block_on(app.shutdown()).into_result().unwrap();
}
