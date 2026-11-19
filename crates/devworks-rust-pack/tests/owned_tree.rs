#![cfg(all(feature = "bundle", target_os = "macos", target_arch = "aarch64"))]

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use devworks_adapters::process::NativeProcessHost;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::process::{ProcessHost, ProcessSpec};
use devworks_extension_protocol::frame::{MAX_FRAME_BYTES, read_frame, write_frame};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn file(path: &Path, data: &str, executable: bool) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
    if executable {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\"'\"'"))
}

fn send(pipe: &mut dyn Write, value: Value) {
    write_frame(&mut &mut *pipe, value.to_string().as_bytes()).unwrap();
    pipe.flush().unwrap();
}

fn request(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn wait_path(path: &Path) {
    let until = Instant::now() + Duration::from_secs(8);
    while !path.is_file() {
        assert!(
            Instant::now() < until,
            "the controlled Cargo descendant never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run(cancel: bool) {
    let fixtures = tempfile::tempdir().unwrap();
    let home = fixtures.path();
    let scratch = home.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let bin = home.join("toolchain/bin");
    let marker = home.join("running-cargo");
    file(
        &bin.join("rust-analyzer"),
        "#!/bin/sh\nprintf '%s\\n' 'rust-analyzer 1.98.0 (fixture)'\n",
        true,
    );
    file(&bin.join("rustc"), "#!/bin/sh\nexit 0\n", true);
    file(
        &home.join("toolchain/lib/rustlib/src/rust/library/std/Cargo.toml"),
        "",
        false,
    );
    file(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\n/bin/sleep 60 &\nprintf '%s\\n' \"$!\" > {}\nwait\n",
            quote(&marker)
        ),
        true,
    );
    file(
        &bin.join("rustup"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' {}\n",
            quote(&bin.join("rust-analyzer"))
        ),
        true,
    );
    let program = env!("CARGO_BIN_EXE_devworks-rust-pack").to_owned();
    let mut spec = ProcessSpec::new(&[program]).unwrap();
    spec.clear_env = true;
    spec.env = vec![
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("PATH".into(), bin.to_string_lossy().into_owned()),
        ("TMPDIR".into(), scratch.to_string_lossy().into_owned()),
    ];
    spec.capture_output = true;
    spec.pipe_stdin = true;
    let mut group = NativeProcessHost.spawn(&spec).unwrap();
    let mut host = group.take_input().unwrap();
    let (mut stdout, _stderr) = group.take_output().unwrap();
    let (tx, rx) = mpsc::channel();
    let listener = std::thread::spawn(move || {
        while let Ok(frame) = read_frame(&mut stdout, MAX_FRAME_BYTES) {
            tx.send(serde_json::from_slice::<Value>(&frame).unwrap())
                .ok();
        }
    });
    let receive = || {
        rx.recv_timeout(Duration::from_secs(8))
            .expect("a native helper response")
    };

    send(
        &mut host,
        request(
            "ses:h:1",
            "host.initialize",
            json!({
                "processSessionId":"ses","offeredMin":{"major":1,"minor":0},"offeredMax":{"major":1,"minor":0},
                "expectedManifestDigest":"a".repeat(64),"requiredFeatures":[],"maxFrameBytes":1048576
            }),
        ),
    );
    assert_eq!(receive()["result"]["kind"], "Initialized");
    send(
        &mut host,
        request(
            "ses:h:2",
            "host.activate",
            json!({
                "processSessionId":"ses","registryRevision":"1","configurationRevision":"1","stateGeneration":"1"
            }),
        ),
    );
    assert_eq!(receive()["result"]["kind"], "Activated");
    let source = json!({"repositoryId":"repo","commit":"b".repeat(40),"captureDigest":null,
        "sourceMode":"Repository","files":[{"unit":"Cargo.toml","language":"cargo","sha256":sha256_hex(b"[package]\n"),"text":"[package]\n"},
        {"unit":"src/lib.rs","language":"rust","sha256":sha256_hex(b"fn demo() {}"),"text":"fn demo() {}"}]}).to_string();
    let artifact = json!({"artifactId":"input","versionId":"v1","sha256":sha256_hex(source.as_bytes()),
        "byteLength":source.len().to_string(),"mediaType":"application/json"});
    send(
        &mut host,
        request(
            "ses:h:3",
            "host.invoke",
            json!({
                "processSessionId":"ses","invocationId":"job","invocationToken":"token",
                "contributionId":"devworks-rust-semantic/index","scope":{"kind":"Workspace"},"allowedOperations":[],
                "registryRevision":"1","stateGeneration":"1","authorityRevision":"1","controlGeneration":"1",
                "operationId":"operation","deadlineMs":45000,"input":artifact
            }),
        ),
    );
    let callback = receive();
    assert_eq!(callback["params"]["call"]["kind"], "ArtifactRead");
    send(
        &mut host,
        json!({"jsonrpc":"2.0","id":callback["id"],"result":{
            "kind":"ArtifactChunk","artifact":artifact,"offset":"0","data":URL_SAFE_NO_PAD.encode(&source),"eof":true
        }}),
    );
    wait_path(&marker);
    if cancel {
        send(
            &mut host,
            request(
                "ses:h:4",
                "host.cancel",
                json!({
                    "processSessionId":"ses","invocationId":"job","cancellationRevision":"1"
                }),
            ),
        );
        assert_eq!(receive()["result"]["kind"], "CancelAcknowledged");
        assert_eq!(
            receive()["result"]["outcome"]["kind"],
            "CancelledBeforeEffect"
        );
    }
    drop(host); // host loses its pipe on crash
    let until = Instant::now() + Duration::from_secs(8);
    while !group.poll().unwrap().empty {
        assert!(
            Instant::now() < until,
            "a descendant survived the helper's stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    listener.join().unwrap();
    let leftover: Vec<_> = std::fs::read_dir(&scratch).unwrap().collect();
    assert!(leftover.is_empty(), "{leftover:?}");
    assert!(
        rx.try_iter()
            .all(|frame| frame["method"] != "package.event")
    );
}

#[test]
fn index_cancellation_reaps_the_owned_compiler_tree() {
    run(true);
}

#[test]
fn losing_the_host_channel_reaps_the_owned_compiler_tree_without_a_result() {
    run(false);
}
