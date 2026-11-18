use super::*;
use devworks_core::protocol::{ProjectId, RootId};
use devworks_test_support::{FaultFiles, FileOp};
use std::{
    io::Read,
    sync::{Mutex, mpsc},
};
type Gate = (mpsc::SyncSender<()>, mpsc::Receiver<()>);
struct GatedReader {
    inner: Box<dyn Read + Send>,
    gate: Option<Gate>,
}
impl Read for GatedReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if let Some((entered, release)) = self.gate.take() {
            entered.send(()).map_err(std::io::Error::other)?;
            release
                .recv_timeout(std::time::Duration::from_secs(3))
                .map_err(std::io::Error::other)?;
        }
        self.inner.read(out)
    }
}

fn fixture(root: &str) -> (Fixture, Arc<FaultFiles>) {
    let h = Harness::new();
    let store = h.open();
    common::seed_root(&*store, root);
    let ctx = principal(&*store, "body-author", PrincipalKind::Human).with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(RootId(id(root))),
        None,
    );
    let files = FaultFiles::over(Arc::new(NativeArtifactFiles::new(
        h.root.clone(),
        h.root.with_extension("cache"),
    )));
    let artifacts = Arc::new(ArtifactService::new(
        files.clone(),
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
    (
        Fixture {
            h,
            store,
            artifacts,
            index,
            ctx,
        },
        files,
    )
}
/// Makes the next controlled open return a reader whose first read waits for the release.
fn gate(files: &FaultFiles) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (entered, wait) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    let slot = Mutex::new(Some((entered, blocked)));
    files.wrap_reader(FileOp::OpenControlled, move |_, reader| {
        Box::new(GatedReader {
            inner: reader,
            gate: slot.lock().unwrap().take(),
        })
    });
    (wait, release)
}
fn pause(store: &dyn ApplicationStore, root: RootId) {
    write(store, move |tx| {
        let old = tx.runs().root_control(&root)?;
        let mut current = old
            .clone()
            .unwrap_or_else(|| StoredRootControl::initial(root));
        current.desired = DesiredExecution::Paused;
        if let Some(old) = old {
            current.revision += 1;
            tx.runs().update_root_control(&current, old.revision)
        } else {
            tx.runs().insert_root_control(&current)
        }
    })
    .unwrap();
}
#[test]
fn pause_after_actual_body_read_blocks_publication_without_clearing_hold_and_continue_can_reconcile()
 {
    let (f, files) = fixture("pause-root");
    let (owner, op) = f.source("paused-body", "# Policy\nexact body");
    let (wait, release) = gate(&files);
    let index = f.index.clone();
    let actual = op.clone();
    let worker = std::thread::spawn(move || index.run_index(&actual, &Cancellation::new(), None));
    wait.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    pause(&*f.store, RootId(id("pause-root")));
    release.send(()).unwrap();
    assert!(matches!(worker.join().unwrap(), Err(StorageError::Busy)));
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
    let control = read(&*f.store, |r| {
        r.runs().root_control(&RootId(id("pause-root")))
    })
    .unwrap();
    assert_eq!(control.desired, DesiredExecution::Paused);
    assert_eq!(control.stop_epoch, 0);
    write(&*f.store, move |tx| {
        let mut resumed = control.clone();
        resumed.desired = DesiredExecution::Enabled;
        resumed.revision += 1;
        tx.runs().update_root_control(&resumed, control.revision)
    })
    .unwrap();
    assert!(matches!(
        f.index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
}
#[test]
fn relinked_task_cannot_publish_under_old_parent_or_waive_new_parent_pause() {
    let (mut f, files) = fixture("feature-a");
    common::seed_root(&*f.store, "feature-b");
    devworks_test_support::seed::work(
        &*f.store,
        &[
            devworks_test_support::seed::WorkSeed::task("task", "project-a", "feature-a")
                .title("Task")
                .created_at(0),
        ],
    )
    .unwrap();
    f.ctx = f.ctx.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(RootId(id("task"))),
        None,
    );
    let (owner, op) = f.source("relinked-body", "authored body");
    let original = op.payload["controlRoots"].clone();
    let (wait, release) = gate(&files);
    let index = f.index.clone();
    let actual = op.clone();
    let worker = std::thread::spawn(move || index.run_index(&actual, &Cancellation::new(), None));
    wait.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    write(&*f.store, |tx| {
        let mut task = tx.work().work(&id("task"))?.unwrap();
        task.parent_feature_id = Some(id("feature-b"));
        task.revision += 1;
        task.scope_revision += 1;
        tx.work().update_work(&task, 1)
    })
    .unwrap();
    pause(&*f.store, RootId(id("feature-b")));
    release.send(()).unwrap();
    assert!(matches!(
        worker.join().unwrap(),
        Err(StorageError::ScopeDenied)
    ));
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
    assert_eq!(
        read(&*f.store, |r| r.operations().operation(&op.id))
            .unwrap()
            .payload["controlRoots"],
        original
    );
    assert!(
        original
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r[0] == "feature-a")
    );
    assert_eq!(
        read(&*f.store, |r| r
            .runs()
            .root_control(&RootId(id("feature-b"))))
        .unwrap()
        .desired,
        DesiredExecution::Paused
    );
}
