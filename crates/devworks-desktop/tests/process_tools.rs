//! Native process-tool route: one host-owned executable, durable effect, typed completion and
//! bounded paged output through the same desktop backend used by agent tool ingress.
//! This fixture invokes the Unix `printf` and `sleep` commands; Windows compiles the service
//! and runs its existing native process-supervisor checks in the platform CI lane.
#![cfg(unix)]
mod common;
use common::*;
use devworks_core::ports::storage::{
    ProcessPhase, ReadOptions, StoredRepository, WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use devworks_core::protocol::{CommandReply, Id};
use devworks_desktop::ops;
use serde_json::json;
use std::time::{Duration, Instant};

/// Names `names` in the workspace's process executables, as the Settings screen saves it.
fn allow_executables(backend: &dyn devworks_desktop::backend::Backend, names: &[&str]) {
    let key = devworks_core::settings::catalog::PROCESS_EXECUTABLES_KEY;
    let (revision, _) = ops::settings_entries(
        backend.query("settings.read_effective", &ops::settings_read(backend)),
    )
    .expect("workspace settings");
    let d = devworks_core::settings::catalog::descriptor(key).unwrap();
    let body = json!({"scope": {"kind": "Workspace"},
        "expectedRevision": revision.to_string(), "entries": [
        {"key": key, "value": {"schema": d.schema_ref(), "value": names}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(backend, "settings.save_revision", body),
    ))
    .expect("process executables saved");
}

#[test]
fn native_execute_has_receipt_identity_completion_and_bounded_output() {
    let _case = native_runtime_case();
    let root = Root::new("process-tools");
    let repository = root.dir("repository");
    std::fs::create_dir_all(repository.join("work")).expect("working directory");
    let repository = repository.canonicalize().expect("canonical repository");
    let engine = start(&root);
    let row = StoredRepository {
        id: Id::new("process-repository").unwrap(),
        location: repository.to_string_lossy().into_owned(),
        name: "process repository".into(),
        remote_url: None,
        primary: true,
        lifecycle: "Active".into(),
        languages: Vec::new(),
        revision: 1,
        registered_at_ms: 1,
    };
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| tx.projects().insert_repository(&row),
    )
    .expect("registered root");
    let backend = engine.backend();
    let spec = json!({
        "executableRef":"printf", "args":["process-output"],
        "workingDirectory":{"rootId":"process-repository","relative":"work"},
        "environment":[], "profile":"Full", "timeoutMs":"5000", "outputLimitBytes":"7"
    });
    // `printf` is on PATH but not a configured process executable, so it cannot be selected
    // until the workspace setting names it; the saved change applies without a restart.
    let unlisted = backend
        .command(
            "process.execute",
            &ops::command_frame(&*backend, "process.execute", json!({"spec":spec})),
        )
        .expect("unlisted reply");
    assert!(
        matches!(&unlisted, CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })),
        "{unlisted:?}"
    );
    allow_executables(&*backend, &["printf", "sleep", "cargo"]);
    let spec = json!({
        "executableRef":"printf", "args":["process-output"],
        "workingDirectory":{"rootId":"process-repository","relative":"work"},
        "environment":[], "profile":"Full", "timeoutMs":"5000", "outputLimitBytes":"7"
    });
    let receipt = backend
        .command(
            "process.execute",
            &ops::command_frame(&*backend, "process.execute", json!({"spec":spec})),
        )
        .expect("gateway command");
    let operation = match receipt {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("expected accepted process, got {other:?}"),
        },
        other => panic!("expected process receipt, got {other:?}"),
    };
    // The receipt's opaque ID is also the reserved process ID, so output and cancellation can
    // name it before the launch has completed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = loop {
        match backend
            .process_completion(&operation)
            .expect("scoped completion")
        {
            Some(OperationCompletion::Succeeded { result, .. }) => break result,
            Some(other) => panic!("process failed: {other:?}"),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("process completion timed out"),
        }
    };
    let record = match result {
        OperationResult::ProcessExecute { value } => value,
        other => panic!("wrong result: {other:?}"),
    };
    assert_eq!(record.id, operation.0);
    assert_eq!(record.exit_code, Some(0));
    assert!(record.truncated);
    let page = ops::query_result(backend.query(
        "process.output",
        &ops::query_frame(
            &*backend,
            "process.output",
            json!({
                "processId": record.id, "afterBytes":"0", "maxBytes":64
            }),
        ),
    ))
    .expect("output page")
    .0;
    let OperationResult::ProcessOutput { value: page } = page else {
        panic!("wrong page")
    };
    assert!(page.complete);
    assert!(page.truncated);
    assert_eq!(page.first_byte.get(), 0);
    assert_eq!(page.next_byte.get(), 7);
    assert_eq!(page.content.0, "cHJvY2Vzcw==");
    let binary = backend
        .command(
            "process.execute",
            &ops::command_frame(
                &*backend,
                "process.execute",
                json!({"spec": {
                    "executableRef":"printf", "args":["\\377"],
                    "workingDirectory":{"rootId":"process-repository","relative":"work"},
                    "environment":[{"name":"MODE","value":"test"}], "profile":"Full", "timeoutMs":"5000", "outputLimitBytes":"64"
                }}),
            ),
        )
        .expect("binary receipt");
    let binary_id = match binary {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("binary process refused: {other:?}"),
        },
        other => panic!("expected binary receipt: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match backend
            .process_completion(&binary_id)
            .expect("binary completion")
        {
            Some(OperationCompletion::Succeeded { .. }) => break,
            Some(other) => panic!("binary process failed: {other:?}"),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("binary process timed out"),
        }
    }
    let binary_page = ops::query_result(backend.query(
        "process.output",
        &ops::query_frame(
            &*backend,
            "process.output",
            json!({"processId":binary_id.0,"afterBytes":"0","maxBytes":64}),
        ),
    ))
    .expect("binary output")
    .0;
    assert!(
        matches!(&binary_page, OperationResult::ProcessOutput { value }
        if value.content.0 == "/w==" && value.next_byte.get() == 1),
        "{binary_page:?}"
    );

    let restricted = backend.command("process.execute", &ops::command_frame(
        &*backend, "process.execute", json!({"spec": {
            "executableRef":"printf", "args":["refused"],
            "workingDirectory":{"rootId":"process-repository","relative":"work"},
            "environment":[], "profile":"Project", "timeoutMs":"1000", "outputLimitBytes":"64"
        }}),
    )).expect("restricted-profile reply");
    assert!(matches!(restricted, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Refused { .. })));

    let sleeping = backend
        .command(
            "process.execute",
            &ops::command_frame(
                &*backend,
                "process.execute",
                json!({"spec": {
                    "executableRef":"sleep", "args":["30"],
                    "workingDirectory":{"rootId":"process-repository","relative":"work"},
                    "environment":[], "profile":"Full", "timeoutMs":"60000", "outputLimitBytes":"64"
                }}),
            ),
        )
        .expect("sleep receipt");
    let sleeping = match sleeping {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("expected accepted sleeper: {other:?}"),
        },
        other => panic!("expected sleeper receipt: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let running = loop {
        let process = read_snapshot(
            &**engine.host().runtime().store(),
            &ReadOptions::default(),
            |snapshot| snapshot.resources().process(&sleeping.0),
        )
        .expect("owned process read");
        if let Some(process) = process
            && process.phase == ProcessPhase::Running
        {
            break process;
        }
        assert!(Instant::now() < deadline, "owned sleeper did not launch");
        std::thread::sleep(Duration::from_millis(20));
    };
    let second = backend
        .command(
            "process.execute",
            &ops::command_frame(
                &*backend,
                "process.execute",
                json!({"spec": {
                    "executableRef":"sleep", "args":["30"],
                    "workingDirectory":{"rootId":"process-repository","relative":"work"},
                    "environment":[], "profile":"Full", "timeoutMs":"60000", "outputLimitBytes":"64"
                }}),
            ),
        )
        .expect("second sleep receipt");
    let second_id = match second {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("second sleeper refused: {other:?}"),
        },
        other => panic!("expected second sleeper receipt: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let second_running = loop {
        let process = read_snapshot(
            &**engine.host().runtime().store(),
            &ReadOptions::default(),
            |snapshot| snapshot.resources().process(&second_id.0),
        )
        .expect("second process read");
        if let Some(process) = process
            && process.phase == ProcessPhase::Running
        {
            break process;
        }
        assert!(Instant::now() < deadline, "second sleeper did not launch");
        std::thread::sleep(Duration::from_millis(20));
    };
    // Both execute slots remain occupied here; cancellation must use its own consumer.
    let cancelled = backend
        .command(
            "process.cancel",
            &ops::command_frame(
                &*backend,
                "process.cancel",
                json!({"processId":sleeping.0,"expectedRevision":running.revision.to_string()}),
            ),
        )
        .expect("cancel receipt");
    let cancellation = match cancelled {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("expected accepted cancellation: {other:?}"),
        },
        other => panic!("expected cancellation receipt: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match backend
            .process_completion(&cancellation)
            .expect("cancel completion")
        {
            Some(OperationCompletion::Succeeded {
                result: OperationResult::ProcessCancel { value },
                ..
            }) => {
                assert_eq!(value.id, sleeping.0);
                break;
            }
            Some(other) => panic!("cancellation failed: {other:?}"),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("cancellation completion timed out"),
        }
    }
    let second_cancel = backend
        .command(
            "process.cancel",
            &ops::command_frame(
                &*backend,
                "process.cancel",
                json!({"processId":second_id.0,
            "expectedRevision":second_running.revision.to_string()}),
            ),
        )
        .expect("second cancellation receipt");
    let second_cancel_id = match second_cancel {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("second cancellation refused: {other:?}"),
        },
        other => panic!("expected second cancellation receipt: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match backend
            .process_completion(&second_cancel_id)
            .expect("second cancellation completion")
        {
            Some(OperationCompletion::Succeeded {
                result: OperationResult::ProcessCancel { value },
                ..
            }) => {
                assert_eq!(value.id, second_id.0);
                break;
            }
            Some(other) => panic!("second cancellation failed: {other:?}"),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("second cancellation timed out"),
        }
    }

    // The dependency command binds a live artifact to the exact manifest in a registered
    // repository and runs the package manager with project-local home/cache/target paths.
    // Its own workspace root: the data root sits under this repository's `target/`.
    let manifest = b"[package]\nname = \"dw-process-tool-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";
    std::fs::write(repository.join("Cargo.toml"), manifest).expect("manifest");
    std::fs::create_dir_all(repository.join("src")).expect("crate source directory");
    std::fs::write(
        repository.join("src/lib.rs"),
        "pub fn value() -> u8 { 7 }\n",
    )
    .expect("crate source");
    let manifest_file = repository.join("Cargo.toml");
    let ticket = backend
        .mint_workspace_upload(&manifest_file)
        .expect("manifest ticket");
    let digest = devworks_core::fingerprint::sha256_hex(manifest);
    let saved = ops::saved_artifact(backend.command(
        "artifacts.save",
        &ops::save_workspace_artifact(
            &*backend,
            ticket.as_str(),
            "dependency manifest",
            "text/plain",
            &digest,
            manifest.len() as u64,
            &[],
        ),
    ))
    .expect("saved manifest");
    let installed = backend
        .command(
            "process.install_project_dependencies",
            &ops::command_frame(
                &*backend,
                "process.install_project_dependencies",
                json!({
                    "repositoryId":"process-repository", "expectedRevision":"1",
                    "manifest":saved.artifact, "profile":"Full"
                }),
            ),
        )
        .expect("install receipt");
    let install_id = match installed {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("project install refused: {other:?}"),
        },
        other => panic!("expected install receipt: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match backend
            .process_completion(&install_id)
            .expect("install completion")
        {
            Some(OperationCompletion::Succeeded {
                result: OperationResult::ProcessInstallProjectDependencies { value },
                ..
            }) => {
                assert_eq!(value.id, install_id.0);
                if value.exit_code != Some(0) {
                    let output = ops::query_result(backend.query(
                        "process.output",
                        &ops::query_frame(
                            &*backend,
                            "process.output",
                            json!({"processId":install_id.0,"afterBytes":"0","maxBytes":8192}),
                        ),
                    ))
                    .expect("failed install output")
                    .0;
                    panic!("project install exited {:?}: {output:?}", value.exit_code);
                }
                break;
            }
            Some(other) => panic!("project install failed: {other:?}"),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("project install timed out"),
        }
    }
    assert!(
        repository
            .join("target/debug/libdw_process_tool_fixture.rlib")
            .exists()
    );
    let output = ops::query_result(backend.query(
        "process.output",
        &ops::query_frame(
            &*backend,
            "process.output",
            json!({"processId":install_id.0,"afterBytes":"0","maxBytes":128}),
        ),
    ))
    .expect("install output")
    .0;
    assert!(matches!(output, OperationResult::ProcessOutput { value } if value.complete));
    let restricted_install = backend
        .command(
            "process.install_project_dependencies",
            &ops::command_frame(
                &*backend,
                "process.install_project_dependencies",
                json!({
                    "repositoryId":"process-repository", "expectedRevision":"1",
                    "manifest":saved.artifact, "profile":"Project"
                }),
            ),
        )
        .expect("restricted install reply");
    assert!(
        matches!(restricted_install, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Refused { .. }))
    );
    std::fs::write(
        &manifest_file,
        b"[package]\nname = \"changed\"\nversion = \"0.1.0\"\n",
    )
    .expect("changed manifest");
    let changed_install = backend
        .command(
            "process.install_project_dependencies",
            &ops::command_frame(
                &*backend,
                "process.install_project_dependencies",
                json!({
                    "repositoryId":"process-repository", "expectedRevision":"1",
                    "manifest":saved.artifact, "profile":"Full"
                }),
            ),
        )
        .expect("changed install reply");
    assert!(
        matches!(changed_install, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Refused { .. }))
    );

    let package_bytes = br#"{"manager":"example","package":"tool"}"#;
    let package_file = root.file("system-install-spec.json", package_bytes);
    let ticket = backend
        .mint_workspace_upload(&package_file)
        .expect("upload ticket");
    let digest = devworks_core::fingerprint::sha256_hex(package_bytes);
    let saved = ops::saved_artifact(backend.command(
        "artifacts.save",
        &ops::save_workspace_artifact(
            &*backend,
            ticket.as_str(),
            "system install package spec",
            "application/json",
            &digest,
            package_bytes.len() as u64,
            &[],
        ),
    ))
    .expect("saved package spec");
    let proposed = ops::command_result(backend.command(
        "process.request_system_install",
        &ops::command_frame(
            &*backend,
            "process.request_system_install",
            json!({
                "packageSpec": saved.artifact, "contentDigest": digest,
                "reason": "Review exact system package request"
            }),
        ),
    ))
    .expect("system install proposal");
    let OperationResult::ProcessRequestSystemInstall { value: proposal } = proposed else {
        panic!("wrong system install result")
    };
    assert!(matches!(
        proposal.state,
        devworks_core::protocol::records::ApprovalState::Pending
    ));
    let decided = ops::command_result(backend.human_decision(&ops::command_frame(
        &*backend,
        "approvals.record_human_decision",
        json!({
            "proposalId": proposal.id,
            "expectedRevision": proposal.revision,
            "contentDigest": proposal.content_digest,
            "decision":"Approve",
            "rationale":"Consent to the reviewed request; no install was run"
        }),
    )))
    .expect("trusted human decision");
    let OperationResult::ApprovalsRecordHumanDecision { value: decision } = decided else {
        panic!("wrong approval result")
    };
    assert!(matches!(
        decision.state,
        devworks_core::protocol::records::ApprovalState::Approved
    ));
}
