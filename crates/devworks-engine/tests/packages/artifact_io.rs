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

fn steps(calls: Vec<(&str, Value)>) -> Value {
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
        ("read-1", read_call(json!("$input"), None)),
        ("append-1", append("copy", json!("$lastData"))),
        (
            "read-2",
            read_call(json!("$input"), Some(ARTIFACT_CHUNK_BYTES)),
        ),
        ("append-2", append("copy", json!("$lastData"))),
        ("publish", publish("copy", &sha256_hex(&content))),
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
fn an_artifact_outside_the_invocation_is_refused_and_every_size_is_bounded() {
    let _s = serial();
    let a = App::new();
    let name = pack(&a, "bound-pack");
    let input = a.save(&a.user, b"{}");
    let other = a.save(&a.user, b"someone else's");
    let full = base64url(&vec![7u8; ARTIFACT_CHUNK_BYTES]);
    let mut calls = vec![
        // Not an artifact of this invocation.
        ("foreign", read_call(other, None)),
        // A chunk larger than one chunk's bytes.
        (
            "oversized",
            append(
                "big",
                json!(base64url(&vec![1u8; ARTIFACT_CHUNK_BYTES + 1])),
            ),
        ),
        // A digest the written bytes do not have.
        ("small", append("small", json!(base64url(b"abc")))),
        ("mismatch", publish("small", &"0".repeat(64))),
    ];
    // More pending writes than an invocation may hold.
    let ids: Vec<String> = (0..MAX_PENDING).map(|n| format!("w{n}")).collect();
    for id in &ids[..MAX_PENDING - 1] {
        calls.push(("open", append(id, json!(base64url(b"x")))));
    }
    calls.push(("too-many", append("one-more", json!(base64url(b"x")))));
    // A write past the largest artifact: `w0` already holds one byte, so the last full chunk
    // would take it one byte over.
    let fills = MAX_ARTIFACT / ARTIFACT_CHUNK_BYTES - 1;
    for _ in 0..fills {
        calls.push(("fill", append(&ids[0], json!(full))));
    }
    calls.push(("over", append(&ids[0], json!(full))));
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
    expected.extend((0..MAX_PENDING - 1).map(|_| "call:open:ArtifactAppended".to_owned()));
    expected.push("call:too-many:PermissionDenied".to_owned());
    expected.extend((0..fills).map(|_| "call:fill:ArtifactAppended".to_owned()));
    expected.push("call:over:PermissionDenied".to_owned());
    assert_eq!(progress.len(), expected.len(), "{progress:?}");
    assert_eq!(progress, expected);
}
