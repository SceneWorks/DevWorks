//! Native host, SQLite, approval replay and the shared desktop effect consumer. The scripted
//! platform records input instead of touching the user's real screen, focus or keyboard.
mod common;
use common::*;
use devworks_core::ports::desktop::{
    AccessibilityNode, AccessibilitySnapshot, DesktopAvailability, DesktopCapabilities,
    DesktopFault, DesktopPlatform,
};
use devworks_core::ports::storage::{ReadOptions, read_snapshot};
use devworks_core::protocol::records::{DesktopInput, DesktopRecord, ItemState};
use devworks_core::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use devworks_core::protocol::{CommandReply, OperationState};
use devworks_desktop::backend::Backend;
use devworks_desktop::{decision_ops as dops, ops};
use image::{DynamicImage, ImageFormat, RgbaImage};
use serde_json::json;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct ScriptedDesktop {
    input: Mutex<Vec<DesktopInput>>,
    captures: Mutex<usize>,
    pointer_available: AtomicBool,
}
impl Default for ScriptedDesktop {
    fn default() -> Self {
        Self {
            input: Mutex::default(),
            captures: Mutex::default(),
            pointer_available: AtomicBool::new(false),
        }
    }
}
impl DesktopPlatform for ScriptedDesktop {
    fn capabilities(&self) -> DesktopCapabilities {
        DesktopCapabilities {
            target: "test-scripted".into(),
            capture: DesktopAvailability::Available,
            input: if self.pointer_available.load(Ordering::SeqCst) {
                DesktopAvailability::Available
            } else {
                DesktopAvailability::Unsupported("pointer-not-available")
            },
            accessibility: DesktopAvailability::Available,
        }
    }
    fn capture_png(&self) -> Result<Vec<u8>, DesktopFault> {
        *self.captures.lock().unwrap() += 1;
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::new(2, 2))
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|_| DesktopFault::NativeFailure)?;
        Ok(png.into_inner())
    }
    fn inject(&self, action: &DesktopInput) -> Result<(), DesktopFault> {
        self.input.lock().unwrap().push(action.clone());
        Ok(())
    }
    fn accessibility_snapshot(&self) -> Result<AccessibilitySnapshot, DesktopFault> {
        Ok(AccessibilitySnapshot {
            platform: "scripted".into(),
            nodes: vec![AccessibilityNode {
                target_id: "test:button".into(),
                parent_id: None,
                role: "button".into(),
                name: "Continue".into(),
                description: None,
                enabled: true,
                actions: vec!["press".into()],
            }],
            truncated: false,
        })
    }
}

fn at_revision(frame: Vec<u8>, revision: u64) -> Vec<u8> {
    let mut frame: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    frame["schemaRevision"] = json!(revision);
    serde_json::to_vec(&frame).unwrap()
}

fn inspect(backend: &dyn Backend) -> DesktopRecord {
    let (result, _) = ops::query_result(backend.query(
        "desktop.inspect",
        &ops::query_frame(backend, "desktop.inspect", json!({})),
    ))
    .expect("desktop inspect");
    let OperationResult::DesktopInspect { value } = result else {
        panic!("wrong desktop result: {result:?}");
    };
    value
}

fn decision(reply: CommandReply<DomainDecision>) -> DomainDecision {
    match reply {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected recorded command: {other:?}"),
    }
}

#[test]
fn control_requires_human_ok_and_live_fenced_consent() {
    let _case = native_runtime_case();
    let root = Root::new("desktop-control");
    let platform = Arc::new(ScriptedDesktop::default());
    let mut configuration = config(&root.0);
    configuration.desktop_platform = Some(platform.clone());
    let engine = devworks_desktop::host::Engine::start(configuration).expect("runtime boot");
    let backend = engine.backend();
    let before = inspect(&*backend);
    assert!(matches!(before.state, ItemState::Disabled));
    assert!(before.lease_id.is_none());
    let request = json!({"desktopId":before.desktop_id,"expectedRevision":before.revision,
        "reason":"Test the local UI", "leaseMs":"30000"});
    let refused = decision(
        backend
            .command(
                "desktop.request_control",
                &ops::command_frame(&*backend, "desktop.request_control", request.clone()),
            )
            .expect("direct takeover reply"),
    );
    assert!(matches!(refused, DomainDecision::Refused { fault }
        if fault.code == devworks_core::protocol::DomainErrorCode::ApprovalRequired));
    assert!(platform.input.lock().unwrap().is_empty());

    let decoded = devworks_core::services::approvals::requests::decode(
        "desktop.request_control",
        1,
        &request,
        true,
    )
    .expect("selected saved operation");
    let proposal = dops::decided(backend.command(
        "approvals.propose",
        &dops::command(
            &*backend,
            "approvals.propose",
            json!({
                "scope":{"kind":"Workspace"},"operationKind":"desktop.request_control",
                "target":{"id":before.desktop_id,"revision":before.revision},
                "contentDigest":decoded.fingerprint.as_str(),"summary":"Request desktop control",
                "payload":{"kind":"SavedOperation","schemaRevision":"1","input":request}
            }),
        ),
    ))
    .expect("staged exact proposal");
    assert!(platform.input.lock().unwrap().is_empty());
    dops::decided(backend.human_decision(&dops::command(
        &*backend,
        "approvals.record_human_decision",
        json!({
            "proposalId":proposal.id,"expectedRevision":proposal.revision,
            "contentDigest":proposal.content_digest,"decision":"Approve",
            "rationale":"OK: test desktop takeover"
        }),
    )))
    .expect("trusted Human OK");

    let deadline = Instant::now() + Duration::from_secs(15);
    let held = loop {
        let record = inspect(&*backend);
        if matches!(record.state, ItemState::Enabled)
            && record.consent_id.as_ref() == Some(&proposal.id)
        {
            break record;
        }
        assert!(
            Instant::now() < deadline,
            "approved desktop control did not resume: {record:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let lease_id = held.lease_id.clone().expect("held lease");
    // The lease was granted while only accessibility control was available.
    platform.pointer_available.store(true, Ordering::SeqCst);
    let second_request = json!({"desktopId":held.desktop_id,"expectedRevision":held.revision,
        "reason":"Second takeover while automation can inject input", "leaseMs":"30000"});
    let decoded = devworks_core::services::approvals::requests::decode(
        "desktop.request_control",
        1,
        &second_request,
        true,
    )
    .expect("second saved operation");
    let second = dops::decided(backend.command(
        "approvals.propose",
        &dops::command(
            &*backend,
            "approvals.propose",
            json!({
                "scope":{"kind":"Workspace"},"operationKind":"desktop.request_control",
                "target":{"id":held.desktop_id,"revision":held.revision},
                "contentDigest":decoded.fingerprint.as_str(),"summary":"Second desktop control",
                "payload":{"kind":"SavedOperation","schemaRevision":"1","input":second_request}
            }),
        ),
    ))
    .expect("second proposal");
    assert!(
        dops::decided(backend.human_decision(&dops::command(
            &*backend,
            "approvals.record_human_decision",
            json!({
                "proposalId":second.id,"expectedRevision":second.revision,
                "contentDigest":second.content_digest,"decision":"Approve",
                "rationale":"Synthetic event must not grant trusted approval"
            }),
        )))
        .is_err(),
        "automation must not mint a trusted Human decision"
    );
    let lease = read_snapshot(
        &**engine.host().runtime().store(),
        &ReadOptions::default(),
        |snapshot| snapshot.resources().lease(&lease_id),
    )
    .expect("lease read")
    .expect("lease row");
    let renewal = decision(
        backend
            .command(
                "resources.renew",
                &ops::command_frame(
                    &*backend,
                    "resources.renew",
                    json!({
                        "leaseId":lease_id,"expectedFence":lease.fence.to_string(),"leaseMs":"60000"
                    }),
                ),
            )
            .expect("generic renewal reply"),
    );
    assert!(matches!(renewal, DomainDecision::Refused { fault }
        if fault.code == devworks_core::protocol::DomainErrorCode::PermissionDenied));
    let action = json!({"desktopId":held.desktop_id,"leaseId":lease_id,"expectedFence":lease.fence.to_string(),
        "consentId":proposal.id,"expectedDesktopRevision":held.revision,
        "action":{"kind":"Text","text":"hello"}});
    let operation = match decision(
        backend
            .command(
                "desktop.input",
                &ops::command_frame(&*backend, "desktop.input", action.clone()),
            )
            .expect("input reply"),
    ) {
        DomainDecision::Accepted { operation_id } => operation_id,
        other => panic!("expected accepted input: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let row = read_snapshot(
            &**engine.host().runtime().store(),
            &ReadOptions::default(),
            |snapshot| snapshot.operations().operation(&operation),
        )
        .expect("input operation read")
        .expect("input operation");
        if row.state == OperationState::ObservedComplete {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "input did not complete: {:?}",
            row.state
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(platform.input.lock().unwrap().len(), 1);
    assert!(matches!(
        backend
            .desktop_completion(&operation)
            .expect("scoped completion"),
        Some(OperationCompletion::Succeeded {
            result: OperationResult::DesktopInput { .. },
            ..
        })
    ));
    let (target_inspection, _) = ops::query_result(backend.query(
        "desktop.inspect",
        &at_revision(ops::query_frame(&*backend, "desktop.inspect", json!({})), 3),
    ))
    .expect("revision-three accessibility inspection");
    let OperationResult::DesktopInspect {
        value: target_inspection,
    } = target_inspection
    else {
        panic!("wrong accessibility inspection result");
    };
    let tree = target_inspection
        .accessibility
        .expect("native accessibility tree");
    assert_eq!(tree.nodes.0[0].target_id.0, "test:button");
    let target_action = json!({"desktopId":held.desktop_id,"leaseId":lease_id,
        "expectedFence":lease.fence.to_string(),"consentId":proposal.id,
        "expectedDesktopRevision":held.revision,
        "action":{"kind":"Target","targetId":"test:button",
            "snapshotRevision":tree.revision.0,
            "action":{"kind":"Invoke","action":"press"}}});
    let target_operation = match decision(
        backend
            .command(
                "desktop.input",
                &at_revision(
                    ops::command_frame(&*backend, "desktop.input", target_action),
                    3,
                ),
            )
            .expect("target input reply"),
    ) {
        DomainDecision::Accepted { operation_id } => operation_id,
        other => panic!("expected accepted target input: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(
        backend
            .desktop_completion(&target_operation)
            .expect("target completion"),
        Some(OperationCompletion::Succeeded { .. })
    ) {
        assert!(Instant::now() < deadline, "target input did not complete");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(platform.input.lock().unwrap().len(), 2);
    let released = decision(backend.command("desktop.release_control",
        &ops::command_frame(&*backend,"desktop.release_control",json!({
            "desktopId":held.desktop_id,"leaseId":lease_id,"expectedFence":lease.fence.to_string()
        }))).expect("release reply"));
    assert!(matches!(
        released,
        DomainDecision::Applied {
            result: OperationResult::DesktopReleaseControl { .. }
        }
    ));
    assert!(matches!(inspect(&*backend).state, ItemState::Disabled));
    let mut acquire: serde_json::Value = serde_json::from_slice(&ops::command_frame(
        &*backend,
        "resources.acquire",
        json!({
            "resourceId":"desktop","expectedRevision":"1","units":1,
            "leaseMs":"30000","purpose":format!("desktop-consent:{}", proposal.id.as_str())
        }),
    ))
    .expect("resource frame");
    acquire["schemaRevision"] = json!(2);
    let reused = decision(
        backend
            .command("resources.acquire", &serde_json::to_vec(&acquire).unwrap())
            .expect("generic reacquire reply"),
    );
    assert!(matches!(reused, DomainDecision::Refused { fault }
        if fault.code == devworks_core::protocol::DomainErrorCode::PermissionDenied));
    let stale = decision(
        backend
            .command(
                "desktop.input",
                &ops::command_frame(&*backend, "desktop.input", action),
            )
            .expect("stale input reply"),
    );
    assert!(matches!(stale, DomainDecision::Refused { .. }));
    assert_eq!(platform.input.lock().unwrap().len(), 2);

    let after = inspect(&*backend);
    let (capture, _) = ops::query_result(backend.query(
        "desktop.capture",
        &ops::query_frame(
            &*backend,
            "desktop.capture",
            json!({
                "desktopId":after.desktop_id,"expectedRevision":after.revision
            }),
        ),
    ))
    .expect("captured image evidence");
    assert!(matches!(capture, OperationResult::DesktopCapture { value }
        if value.image.media_type == "image/png" && value.desktop.desktop_id == after.desktop_id));
    assert_eq!(*platform.captures.lock().unwrap(), 1);
    assert!(
        ops::query_result(backend.query(
            "desktop.capture",
            &ops::query_frame(
                &*backend,
                "desktop.capture",
                json!({
                    "desktopId":after.desktop_id,"expectedRevision":before.revision
                })
            ),
        ))
        .is_err()
    );
    assert_eq!(*platform.captures.lock().unwrap(), 1);
}
