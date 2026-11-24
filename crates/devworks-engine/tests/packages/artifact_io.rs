//! Artifact bytes over `broker.call`: a package reads its invocation's input in bounded chunks
//! and writes a new artifact its result can name; an artifact outside the invocation is refused,
//! and chunks, pending writes and written artifacts are bounded.
use super::broker::run_calls;
use super::*;
use devworks_core::ports::http::base64url;
use devworks_engine::package_host::artifact_io::{MAX_ARTIFACT, MAX_PENDING};
use devworks_extension_protocol::package::ARTIFACT_CHUNK_BYTES;

fn pack(a: &App, tag: &str) -> String {
    let name = format!("{tag}-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.contributions = vec!["call"];
    package(a.install(&a.user, &spec));
    a.ready(&name);
    name
}

fn read_call(artifact: Value, offset: Option<usize>) -> Value {
    let mut call = json!({"kind": "ArtifactRead", "artifact": artifact});
    if let Some(offset) = offset {
        call["offset"] = json!(offset.to_string());
    }
    call
}

fn append(write: &str, data: Value) -> Value {
    json!({"kind": "ArtifactAppend", "writeId": write, "data": data})
}

fn publish(write: &str, sha256: &str) -> Value {
    json!({"kind": "ArtifactPublish", "writeId": write, "mediaType": "application/octet-stream",
        "sha256": sha256})
}

fn steps(calls: Vec<(String, Value)>) -> Value {
    json!({"calls": calls.into_iter().map(|(k, c)| json!({"actionKey": k, "call": c})).collect::<Vec<_>>()})
}

#[test]
fn a_package_reads_its_input_in_chunks_and_writes_an_artifact_its_result_names() {
    let _s = serial();
    let a = App::new();
    let name = pack(&a, "io-pack");
    // Larger than one chunk, so it takes two reads.
    let content: Vec<u8> = (0..ARTIFACT_CHUNK_BYTES + 4096)
        .map(|i| (i % 251) as u8)
        .collect();
    let input = a.save(&a.user, &content);
    let script = steps(vec![
        ("read-1".to_owned(), read_call(json!("$input"), None)),
        ("append-1".to_owned(), append("copy", json!("$lastData"))),
        (
            "read-2".to_owned(),
            read_call(json!("$input"), Some(ARTIFACT_CHUNK_BYTES)),
        ),
        ("append-2".to_owned(), append("copy", json!("$lastData"))),
        ("publish".to_owned(), publish("copy", &sha256_hex(&content))),
    ]);
    let (progress, outcome) = run_calls(&a, &name, script, &input, Some(a.agent.clone()), vec![]);
    assert_eq!(
        progress,
        vec![
            "call:read-1:ArtifactChunk",
            "call:append-1:ArtifactAppended",
            "call:read-2:ArtifactChunk",
            "call:append-2:ArtifactAppended",
            "call:publish:BrokerCompleted",
        ]
    );
    // The invocation's result names the written artifact, whose bytes are the input's.
    let InvocationOutcome::Completed { artifacts } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(artifacts.len(), 1);
    let reference = serde_json::from_value(serde_json::to_value(&artifacts[0]).unwrap()).unwrap();
    let (written, _) = a
        .app()
        .artifacts()
        .read_exact(&*a.store, &a.agent, &reference, MAX_ARTIFACT as u64)
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(written, content);
}

#[test]
fn an_opted_in_artifact_reader_uses_only_its_live_ordered_stream_credit() {
    let _s = serial();
    let a = App::new();
    let name = format!("credit-pack-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.contributions = vec!["call"];
    spec.mode = "credited-artifacts".into();
    package(a.install(&a.user, &spec));
    a.ready(&name);
    let input = a.save(&a.user, b"abcdef");
    let step = |key, offset, credit| {
        json!({"actionKey":key, "call":read_call(json!("$input"), Some(offset)),
            "credit":credit, "reportBytes":true})
    };
    let script = json!({"calls":[
        step("no-credit", 0, Value::Null),
        step("out-of-order", 0, json!({"sequence":"2","bytes":3})),
        step("bad-token", 0, json!({"sequence":"1","bytes":3,"token":"foreign"})),
        step("bad-session", 0, json!({"sequence":"1","bytes":3,"processSessionId":"other"})),
        step("other-stream", 0, json!({"sequence":"1","bytes":3,"streamId":"other"})),
        step("first", 0, json!({"sequence":"1","bytes":3})),
        step("duplicate", 3, json!({"sequence":"1","bytes":3})),
        step("overflow", 3, json!({"sequence":"2","bytes":65_537})),
        step("second", 3, json!({"sequence":"2","bytes":3})),
        step("eof", 6, Value::Null),
    ]});
    let (progress, _) = run_calls(&a, &name, script, &input, Some(a.agent.clone()), vec![]);
    assert_eq!(
        progress,
        [
            "call:no-credit:Busy",
            "call:out-of-order:Busy",
            "call:bad-token:Busy",
            "call:bad-session:Busy",
            "call:other-stream:Busy",
            "call:first:ArtifactChunk:3",
            "call:duplicate:Busy",
            "call:overflow:Busy",
            "call:second:ArtifactChunk:3",
            "call:eof:ArtifactChunk:0",
        ]
    );
}

#[test]
fn an_artifact_outside_the_invocation_is_refused_and_every_size_is_bounded() {
    let _s = serial();
    let a = App::new();
    let name = pack(&a, "bound-pack");
    let input = a.save(&a.user, b"{}");
    let other = a.save(&a.user, b"someone else's");
    let full = base64url(&vec![7u8; ARTIFACT_CHUNK_BYTES]);
    let mut calls = vec![
        // Not an artifact of this invocation.
        ("foreign".to_owned(), read_call(other, None)),
        // A chunk larger than one chunk's bytes.
        (
            "oversized".to_owned(),
            append(
                "big",
                json!(base64url(&vec![1u8; ARTIFACT_CHUNK_BYTES + 1])),
            ),
        ),
        // A digest the written bytes do not have.
        (
            "small".to_owned(),
            append("small", json!(base64url(b"abc"))),
        ),
        ("mismatch".to_owned(), publish("small", &"0".repeat(64))),
    ];
    // More pending writes than an invocation may hold.
    let ids: Vec<String> = (0..MAX_PENDING).map(|n| format!("w{n}")).collect();
    for id in &ids[..MAX_PENDING - 1] {
        calls.push(("open".to_owned(), append(id, json!(base64url(b"x")))));
    }
    // A retried append (the same action key) is not applied twice: `w0` still holds one byte.
    calls.push(("open".to_owned(), append(&ids[0], json!(base64url(b"x")))));
    calls.push((
        "too-many".to_owned(),
        append("one-more", json!(base64url(b"x"))),
    ));
    // A write past the largest artifact: `w0` already holds one byte, so the last full chunk
    // would take it one byte over.
    let fills = MAX_ARTIFACT / ARTIFACT_CHUNK_BYTES - 1;
    for n in 0..fills {
        calls.push((format!("fill-{n}"), append(&ids[0], json!(full))));
    }
    calls.push(("over".to_owned(), append(&ids[0], json!(full))));
    let (progress, _) = run_calls(
        &a,
        &name,
        steps(calls),
        &input,
        Some(a.agent.clone()),
        vec![],
    );
    let mut expected = vec![
        "call:foreign:PermissionDenied".to_owned(),
        "call:oversized:ProtocolViolation".to_owned(),
        "call:small:ArtifactAppended".to_owned(),
        "call:mismatch:ProtocolViolation".to_owned(),
    ];
    expected.extend((0..MAX_PENDING).map(|_| "call:open:ArtifactAppended".to_owned()));
    expected.push("call:too-many:PermissionDenied".to_owned());
    expected.extend((0..fills).map(|n| format!("call:fill-{n}:ArtifactAppended")));
    expected.push("call:over:PermissionDenied".to_owned());
    assert_eq!(progress.len(), expected.len(), "{progress:?}");
    assert_eq!(progress, expected);
}

#[test]
fn a_retried_append_is_applied_once_and_a_changed_retry_is_refused() {
    let _s = serial();
    let a = App::new();
    let name = pack(&a, "retry-pack");
    let input = a.save(&a.user, b"{}");
    let (first, second) = (b"first chunk;".to_vec(), b"second chunk".to_vec());
    let expected = [first.as_slice(), second.as_slice()].concat();
    let script = steps(vec![
        ("one".to_owned(), append("w", json!(base64url(&first)))),
        // Its answer was lost: the identical retry changes nothing.
        ("one".to_owned(), append("w", json!(base64url(&first)))),
        // The same key with other bytes is a different call.
        ("one".to_owned(), append("w", json!(base64url(&second)))),
        ("two".to_owned(), append("w", json!(base64url(&second)))),
        ("publish".to_owned(), publish("w", &sha256_hex(&expected))),
    ]);
    let (progress, outcome) = run_calls(&a, &name, script, &input, Some(a.agent.clone()), vec![]);
    assert_eq!(
        progress,
        vec![
            "call:one:ArtifactAppended",
            "call:one:ArtifactAppended",
            "call:one:Conflict",
            "call:two:ArtifactAppended",
            "call:publish:BrokerCompleted",
        ]
    );
    let InvocationOutcome::Completed { artifacts } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        String::from(artifacts[0].sha256.clone()),
        sha256_hex(&expected)
    );
    let reference = serde_json::from_value(serde_json::to_value(&artifacts[0]).unwrap()).unwrap();
    let (written, _) = a
        .app()
        .artifacts()
        .read_exact(&*a.store, &a.agent, &reference, MAX_ARTIFACT as u64)
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(written, expected);
}

#[test]
fn an_invocation_revoked_between_append_and_publish_publishes_nothing() {
    let _s = serial();
    let a = App::new();
    let name = pack(&a, "revoke-pack");
    let input = a.save(&a.user, b"{}");
    let content = b"never published".to_vec();
    let mut later = json!({"actionKey": "publish", "call": publish("w", &sha256_hex(&content))});
    // The invocation's deadline passes while the helper waits before publishing.
    later["sleepMs"] = json!(1500);
    let script = json!({"calls": [
        {"actionKey": "append", "call": append("w", json!(base64url(&content)))},
        later,
    ]});
    let path = std::env::temp_dir().join(format!("devworks-fixture-{name}-call.json"));
    std::fs::write(&path, script.to_string()).unwrap();
    let h = a
        .app()
        .packages()
        .unwrap()
        .invoke(InvokeRequest {
            package_id: name.clone(),
            contribution_id: format!("{name}/call"),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(input).unwrap(),
            operation_id: id(&a.key("op")),
            deadline: Duration::from_millis(500),
            allowed_operations: vec![],
            kind: InvocationKind::Ordinary,
            principal: Some(a.agent.clone()),
            expected_digest: None,
        })
        .unwrap();
    let _ = h.wait(Duration::from_secs(20));
    // Give a late publish every chance to land.
    std::thread::sleep(Duration::from_millis(2500));
    let _ = std::fs::remove_file(&path);
    let digest = sha256_hex(&content);
    assert_eq!(
        read(&*a.store, |s| s.artifacts().live_references(&digest)),
        0,
        "a revoked invocation published its write"
    );
}
