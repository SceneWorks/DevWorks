//! Update controls and receipts through the native desktop composition and SQLite owner.
mod common;
use common::*;
use devworks_core::ports::storage::{Cancellation, FileDigest};
use devworks_core::ports::update::{
    InstallOwner, Release, ReleaseAsset, ReleaseError, ReleaseFile, ReleaseManifest, ReleasePage,
    ReleaseSource, StagedRelease, asset_name,
};
use devworks_core::protocol::CommandReply;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::services::update::{APPROVE, STATUS};
use devworks_desktop::{app_updates, backend::Backend, frame::fixture, host::Engine};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Feed {
    checks: AtomicUsize,
    downloads: AtomicUsize,
    asset_offline: AtomicBool,
    manifest: ReleaseManifest,
    release: Release,
}

impl Feed {
    fn new() -> (Arc<Feed>, devworks_core::ports::update::RunningApplication) {
        let digest = FileDigest {
            bytes: 2,
            sha256: "b".repeat(64),
        };
        let mut running = app_updates::running();
        running.installed_binary_digest = Some("a".repeat(64));
        let owner = if cfg!(target_os = "macos") {
            InstallOwner::MacApplication
        } else if cfg!(windows) {
            InstallOwner::WindowsPerUser
        } else {
            InstallOwner::LinuxStandalone
        };
        let target = running.target.clone();
        let files = ["manifest.json", "manifest.sig", "bundle"];
        let release = Release {
            id: 9,
            tag: "v1.2.0".into(),
            prerelease: false,
            assets: files
                .into_iter()
                .enumerate()
                .map(|(at, suffix)| ReleaseAsset {
                    id: at as u64 + 100,
                    name: asset_name(&target, suffix),
                    size: 2,
                })
                .collect(),
        };
        let manifest = ReleaseManifest {
            format: 1,
            product: "SceneWorks/DevWorks".into(),
            release_id: release.id,
            tag: release.tag.clone(),
            build: "c".repeat(40),
            target: target.clone(),
            owner,
            archive: digest.clone(),
            archive_asset: asset_name(&target, "bundle"),
            help_digest: app_updates::bundled_help_digest(),
            migrations: running.migrations.clone(),
            files: vec![ReleaseFile {
                path: owner.entrypoint().into(),
                content: digest,
                executable: true,
            }],
            destructive_migration: false,
        };
        manifest.check(&release, &target).unwrap();
        (
            Arc::new(Self {
                checks: AtomicUsize::default(),
                downloads: AtomicUsize::default(),
                asset_offline: AtomicBool::default(),
                manifest,
                release,
            }),
            running,
        )
    }
}

impl ReleaseSource for Feed {
    fn discover(
        &self,
        _: &[ReleasePage],
        _: &Cancellation,
    ) -> Result<Vec<ReleasePage>, ReleaseError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(vec![ReleasePage {
            number: 1,
            etag: Some("native".into()),
            next: None,
            releases: vec![self.release.clone()],
        }])
    }
    fn stage(
        &self,
        release: &Release,
        target: &str,
        _: &Cancellation,
        progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<StagedRelease, ReleaseError> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        if self.asset_offline.load(Ordering::SeqCst) {
            return Err(ReleaseError::rejected("The fixture archive is offline."));
        }
        self.manifest.check(release, target)?;
        progress(1, 2);
        progress(2, 2);
        Ok(StagedRelease {
            manifest: self.manifest.clone(),
            manifest_digest: "d".repeat(64),
        })
    }
    fn verify(&self, _: &StagedRelease, _: &Cancellation) -> Result<(), ReleaseError> {
        Ok(())
    }
}

fn frame(backend: &dyn Backend) -> (Vec<u8>, devworks_core::protocol::Id) {
    let selected = backend.update_status().unwrap().current.unwrap();
    let frame = fixture::command(
        backend,
        APPROVE,
        json!({
            "updateId":selected.record.id,
            "expectedRevision":selected.record.revision,
            "binaryDigest":selected.record.digest,
            "proposalId":selected.proposal_id,
        }),
    );
    (frame, selected.proposal_id)
}

#[test]
fn first_desktop_frame_stages_automatically_and_declining_keeps_native_work_open() {
    let _case = native_runtime_case();
    let root = Root::new("app-update-ui");
    let (feed, running) = Feed::new();
    let mut config = config(&root.0);
    config.update_fixture = Some((feed.clone(), running));
    let engine = Engine::start(config).unwrap();
    assert_eq!(feed.checks.load(Ordering::SeqCst), 0);
    let backend = engine.backend();
    let mut app = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut app);
    assert_eq!(feed.checks.load(Ordering::SeqCst), 1);
    assert_eq!(feed.downloads.load(Ordering::SeqCst), 1);
    backend.poll_updates(false).unwrap();
    assert_eq!(
        feed.checks.load(Ordering::SeqCst),
        1,
        "a due launch check runs once"
    );
    app.get_by_label("DevWorks 0.1.0-test");
    click(&mut app, Role::Button, "DevWorks 1.2.0 available");
    click(&mut app, Role::Button, "Review update");
    click(&mut app, Role::Button, "Keep current version");
    assert!(
        engine
            .host()
            .updates()
            .pending_approval()
            .unwrap()
            .is_none()
    );
    app.get_by_label("DevWorks 0.1.0-test");
    click_nav(&mut app, "All projects");
    assert!(app.state().client().is_some());
    assert!(engine.shutdown().is_some());
}

#[test]
fn generic_and_stale_approval_fail_exact_native_approval_replays_without_a_second_install() {
    let _case = native_runtime_case();
    let root = Root::new("app-update-receipt");
    let (feed, running) = Feed::new();
    let mut config = config(&root.0);
    config.update_fixture = Some((feed, running));
    let engine = Engine::start(config.clone()).unwrap();
    let backend = engine.backend();
    backend.poll_updates(false).unwrap();
    let result = backend
        .query(STATUS, &fixture::query(&*backend, STATUS, json!({})))
        .unwrap();
    assert!(matches!(result,
        DomainQueryReply::Result {result:OperationResult::WorkspaceUpdateStatus{value},..}
            if value.items.0.len()==1 && value.items.0[0].signature_verified
    ));
    let (generic, proposal) = frame(&*backend);
    assert!(matches!(
        backend.command(APPROVE, &generic).unwrap(),
        CommandReply::Recorded {receipt, ..} if !matches!(receipt.decision, DomainDecision::Accepted{..})
    ));
    assert!(
        backend
            .approve_app_update(
                &generic,
                &proposal,
                devworks_core::services::desktop::ControlEpoch::UNKNOWN
            )
            .is_err()
    );
    let (stale, stale_id) = frame(&*backend);
    backend
        .set_update_channel(
            devworks_core::protocol::records::UpdateChannel::Prerelease,
            backend.control_epoch(),
        )
        .unwrap();
    backend.poll_updates(false).unwrap();
    assert!(
        backend
            .approve_app_update(&stale, &stale_id, backend.control_epoch())
            .is_err()
    );
    let (approval, proposal) = frame(&*backend);
    backend
        .approve_app_update(&approval, &proposal, backend.control_epoch())
        .unwrap();
    let operation = engine
        .host()
        .updates()
        .pending_approval()
        .unwrap()
        .unwrap()
        .operation;
    assert!(matches!(
        backend.command(APPROVE, &approval).unwrap(),
        CommandReply::Recorded {receipt, replayed:true, ..}
            if matches!(&receipt.decision, DomainDecision::Accepted {operation_id} if operation_id == &operation)
    ));
    let mut edited: serde_json::Value = serde_json::from_slice(&approval).unwrap();
    edited["body"]["binaryDigest"] = json!("e".repeat(64));
    assert!(!matches!(
        backend.command(APPROVE, &serde_json::to_vec(&edited).unwrap()).unwrap(),
        CommandReply::Recorded {receipt, ..} if matches!(receipt.decision, DomainDecision::Accepted{..})
    ));
    assert!(
        backend
            .approve_app_update(&approval, &proposal, backend.control_epoch())
            .is_err()
    );
    drop(backend);
    assert!(engine.shutdown().is_some());
    let reopened = Engine::start(config).unwrap();
    let status = reopened.backend().update_status().unwrap();
    assert!(status.last_detail.is_some());
    assert!(
        reopened
            .host()
            .updates()
            .pending_approval()
            .unwrap()
            .is_none()
    );
    assert!(reopened.shutdown().is_some());
}

#[test]
fn asset_failure_keeps_the_verified_stage_and_cancellation_settles_before_dispatch() {
    use devworks_core::protocol::OperationState;
    use devworks_desktop::shell::HostControl;
    let _case = native_runtime_case();
    let root = Root::new("app-update-cancellation");
    let (feed, running) = Feed::new();
    let mut settings = config(&root.0);
    settings.update_fixture = Some((feed.clone(), running));
    let mut engine = Engine::start(settings.clone()).unwrap();
    let backend = engine.backend();
    backend.poll_updates(false).unwrap();
    let installed = backend.update_status().unwrap();
    feed.asset_offline.store(true, Ordering::SeqCst);
    backend.poll_updates(true).unwrap();
    let offline = backend.update_status().unwrap();
    assert_eq!(offline.checked_at_ms, installed.checked_at_ms);
    assert_eq!(
        offline.current.as_ref().unwrap().record.id,
        installed.current.as_ref().unwrap().record.id
    );
    assert!(offline.issue.is_some() && !offline.can_install);
    // An explicit channel change clears the failed fetch backoff and the old proposal.
    feed.asset_offline.store(false, Ordering::SeqCst);
    backend
        .set_update_channel(
            devworks_core::protocol::records::UpdateChannel::Prerelease,
            backend.control_epoch(),
        )
        .unwrap();
    backend.poll_updates(false).unwrap();
    let (request, proposal) = frame(&*backend);
    backend
        .approve_app_update(&request, &proposal, backend.control_epoch())
        .unwrap();
    assert!(engine.can_cancel_update());
    assert!(engine.cancel_update());
    drop(backend);
    assert!(engine.shutdown().is_some());
    let reopened = Engine::start(settings).unwrap();
    assert!(
        reopened
            .host()
            .updates()
            .pending_approval()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened
            .backend()
            .update_status()
            .unwrap()
            .last
            .unwrap()
            .state,
        OperationState::CancelledBeforeDispatch
    );
    assert!(reopened.shutdown().is_some());
}
