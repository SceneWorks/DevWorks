//! Negotiated operation schema revisions: per-request selection before authority, strict
//! per-revision readers, revision-bound fingerprints and receipts, and per-revision result
//! projection (in-memory ports; the real SQLite store is exercised by the adapters' tests).
#![allow(clippy::result_large_err)]
mod common;

use common::*;
use devworks_core::admission::{self, AdmissionError, Selection};
use devworks_core::ports::{Decided, QueryAnswer};
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, DomainQueryReply};
use devworks_core::protocol::*;
use devworks_core::registry::{self, Mode, SchemaRevision};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const EDIT: &str = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"key-1","body":{"kind":"projects.edit","projectId":"project-a","expectedRevision":"1","changes":{}}}"#;
const DIGEST: &str = "7692c3ad3540bb803c020b3aee66cd8887123234ea0c6e7143c0add73ff431ed";

fn save(key: &str, revision: Option<u64>, tags: Option<Value>) -> String {
    let mut frame = json!({"protocol": "1.0-draft.1", "requestKey": key, "command": {
        "kind": "Save", "projectId": "project-a", "title": "Notes", "mediaType": "text/plain",
        "source": {"kind": "Upload", "ticketId": "t1"}, "expectedDigest": DIGEST,
        "expectedBytes": "3"}});
    if let Some(r) = revision {
        frame["schemaRevision"] = json!(r);
    }
    if let Some(t) = tags {
        frame["command"]["tags"] = t;
    }
    frame.to_string()
}
fn list(revision: Option<u64>, tags: Option<Value>) -> String {
    let mut frame = json!({"protocol": "1.0-draft.1", "queryId": "q-1",
        "query": {"kind": "List", "projectId": "project-a"}, "limit": 50});
    if let Some(r) = revision {
        frame["schemaRevision"] = json!(r);
    }
    if let Some(t) = tags {
        frame["query"]["tags"] = t;
    }
    frame.to_string()
}
fn decode(
    operation: &str,
    mode: Mode,
    raw: &str,
) -> Result<admission::DecodedRequest, AdmissionError> {
    admission::decode(Selection { operation, mode }, raw.as_bytes()).map_err(|r| r.error)
}
/// A canonical (newest-revision) artifact record: a use case always fills every member.
fn record(tags: &[&str]) -> Value {
    json!({"artifact": {"artifactId": "artifact-a", "versionId": "version-a", "sha256": DIGEST,
        "byteLength": "3", "mediaType": "text/plain"}, "name": "Notes", "createdAtMs": "1",
        "scope": {"kind": "Project", "projectId": "project-a"}, "referenceCount": "0",
        "tags": tags})
}
fn rejected(reply: &DomainCommandReply) -> (&Rejection, &RetryAdvice) {
    match reply {
        CommandReply::Rejected { code, retry, .. } => (code, retry),
        other => panic!("expected rejection, got {other:?}"),
    }
}
/// The recorded decision's result value as wire JSON, and whether it was a replay.
fn recorded(reply: &DomainCommandReply) -> (Value, bool) {
    match reply {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => match &receipt.decision {
            DomainDecision::Applied { result } => (
                serde_json::to_value(result).unwrap()["value"].clone(),
                *replayed,
            ),
            other => panic!("{other:?}"),
        },
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}

#[test]
fn an_unregistered_revision_is_refused_before_authority_with_no_receipt() {
    let h = Harness::new();
    let handler = Commands::new(|_| unreachable!("never admitted"));
    for (operation, frame) in [
        (
            "projects.edit",
            EDIT.replace(r#""mode""#, r#""schemaRevision":2,"mode""#),
        ),
        ("artifacts.save", save("key-1", Some(4), None)),
        ("artifacts.save", save("key-1", Some(65535), None)),
    ] {
        let reply = h
            .gateway()
            .command(
                &context("user-a"),
                operation,
                frame.as_bytes(),
                handler.clone(),
            )
            .unwrap();
        assert_eq!(
            rejected(&reply),
            (&Rejection::UnsupportedSchema, &RetryAdvice::Never),
            "{operation}"
        );
        assert!(matches!(
            decode(operation, Mode::Command, &frame),
            Err(AdmissionError::UnsupportedSchemaRevision { .. })
        ));
    }
    let queries = Queries::new(|_| unreachable!("never admitted"));
    let reply = h
        .gateway()
        .query(
            &context("user-a"),
            "artifacts.read",
            list(Some(2), None).as_bytes(),
            &queries,
        )
        .unwrap();
    assert!(
        matches!(&reply, DomainQueryReply::Rejected { fault, .. } if matches!(fault.code, DomainErrorCode::SchemaMismatch))
    );
    // Nothing reached authority, the store, a use case or the receipt ledger.
    assert_eq!(
        (
            h.authority.calls.get(),
            h.store.touched(),
            handler.calls.get(),
            queries.calls.get(),
            h.store.receipt_count()
        ),
        (0, 0, 0, 0, 0)
    );
    // A malformed member is an invalid envelope, not a revision: other types, values outside
    // 1..=65535 and non-integer spellings of an integer.
    for bad in [
        "\"2\"",
        "0",
        "-1",
        "2.5",
        "null",
        "65536",
        "70000",
        "4294967297",
        "1.0",
        "2e0",
    ] {
        let frame = EDIT.replace(r#""mode""#, &format!(r#""schemaRevision":{bad},"mode""#));
        assert!(
            matches!(
                decode("projects.edit", Mode::Command, &frame),
                Err(AdmissionError::InvalidEnvelope(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn revision_one_and_two_requests_are_admitted_with_revision_bound_fingerprints() {
    // Golden fingerprints recorded before revisions existed: revision-1 intent is unchanged.
    let r1 = decode("artifacts.save", Mode::Command, &save("key-1", None, None)).unwrap();
    assert_eq!(r1.revision, SchemaRevision(1));
    assert_eq!(
        r1.fingerprint.as_str(),
        "0c84e75b89242e7dce2c66be4bf6485390a540db74857b9307260194e021069a"
    );
    let l1 = decode("artifacts.list", Mode::Query, &list(None, None)).unwrap();
    assert_eq!(
        l1.fingerprint.as_str(),
        "17fa731ca6e098ece06a465e516244c2ce6234735aa3234d624c28358cb1dd07"
    );
    // Naming revision 1 is the same intent as naming none.
    let explicit = decode(
        "artifacts.save",
        Mode::Command,
        &save("key-1", Some(1), None),
    )
    .unwrap();
    assert_eq!(explicit.fingerprint, r1.fingerprint);
    // Revision 2 is a different intent even with equal remaining input.
    let r2 = decode(
        "artifacts.save",
        Mode::Command,
        &save("key-1", Some(2), None),
    )
    .unwrap();
    assert_eq!(r2.revision, SchemaRevision(2));
    assert_ne!(r2.fingerprint, r1.fingerprint);
    // Tags are a set: order does not matter and an empty set is no tags.
    let a = decode(
        "artifacts.save",
        Mode::Command,
        &save("k", Some(2), Some(json!(["b", "a"]))),
    )
    .unwrap();
    let b = decode(
        "artifacts.save",
        Mode::Command,
        &save("k", Some(2), Some(json!(["a", "b"]))),
    )
    .unwrap();
    assert_eq!(a.fingerprint, b.fingerprint);
    let empty = decode(
        "artifacts.save",
        Mode::Command,
        &save("k", Some(2), Some(json!([]))),
    )
    .unwrap();
    assert_eq!(empty.fingerprint, r2.fingerprint);
    let filtered = decode(
        "artifacts.list",
        Mode::Query,
        &list(Some(2), Some(json!(["x"]))),
    )
    .unwrap();
    assert_ne!(filtered.fingerprint, l1.fingerprint);
    // Operations without a later revision keep only revision 1.
    assert_eq!(
        registry::find("artifacts.reference").unwrap().revisions,
        [SchemaRevision(1)]
    );
}

#[test]
fn each_revision_has_its_own_strict_reader_and_tag_bounds() {
    // Revision 1 refuses the revision-2 member instead of discarding it (absent or explicit 1).
    for frame in [
        save("k", None, Some(json!(["evidence"]))),
        save("k", Some(1), Some(json!(["evidence"]))),
        save("k", Some(1), Some(json!([]))),
        save("k", None, Some(json!(null))),
    ] {
        assert!(
            matches!(
                decode("artifacts.save", Mode::Command, &frame),
                Err(AdmissionError::Decode(_))
            ),
            "{frame}"
        );
    }
    assert!(matches!(
        decode(
            "artifacts.list",
            Mode::Query,
            &list(None, Some(json!(["evidence"])))
        ),
        Err(AdmissionError::Decode(_))
    ));
    let max: Vec<String> = (0..32).map(|i| format!("{i:0>64}")).collect();
    assert!(
        decode(
            "artifacts.save",
            Mode::Command,
            &save("k", Some(2), Some(json!(max)))
        )
        .is_ok()
    );
    let accented = "é".repeat(64);
    assert!(
        decode(
            "artifacts.save",
            Mode::Command,
            &save("k", Some(2), Some(json!([accented])))
        )
        .is_ok()
    );
    let too_many: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    for tags in [
        json!(too_many),
        json!(["dup", "dup"]),
        json!([""]),
        json!(["x".repeat(65)]),
        json!(["nul\u{0}"]),
        json!([1]),
        json!(null),
    ] {
        assert!(
            matches!(
                decode(
                    "artifacts.save",
                    Mode::Command,
                    &save("k", Some(2), Some(tags.clone()))
                ),
                Err(AdmissionError::Decode(_))
            ),
            "{tags}"
        );
        assert!(
            decode(
                "artifacts.list",
                Mode::Query,
                &list(Some(2), Some(tags.clone()))
            )
            .is_err(),
            "{tags}"
        );
    }
}

#[test]
fn receipts_bind_their_revision_and_replay_under_its_projection() {
    let h = Harness::new();
    let ctx = context("user-a");
    let handler = Commands::new(|_| {
        Ok(Decided {
            decision: DomainDecision::Applied {
                result: result("artifacts.save", record(&["evidence", "release"])),
            },
            operation_id: None,
        })
    });
    let command = |frame: &str| {
        h.gateway()
            .command(&ctx, "artifacts.save", frame.as_bytes(), handler.clone())
            .unwrap()
    };

    // A revision-1 receipt is recorded and replayed under the revision-1 projection, although
    // the revision-2 binding exists and the use case produced the newest shape.
    let (value, replayed) = recorded(&command(&save("key-1", None, None)));
    assert!(!replayed);
    assert_eq!(value, without_tags(record(&[])));
    let (value, replayed) = recorded(&command(&save("key-1", Some(1), None)));
    assert!(replayed);
    assert_eq!(value, without_tags(record(&[])));
    // The same key under revision 2 is a different intent, even with equal remaining input.
    assert_eq!(
        rejected(&command(&save("key-1", Some(2), None))),
        (
            &Rejection::IdempotencyConflict,
            &RetryAdvice::RefreshAndNewRequest
        )
    );

    // A revision-2 receipt carries the revision-2 member and replays with it; it cannot be
    // replayed through revision 1 to drop it.
    let tagged = save("key-2", Some(2), Some(json!(["release", "evidence"])));
    let (value, replayed) = recorded(&command(&tagged));
    assert!(!replayed);
    assert_eq!(value, record(&["evidence", "release"]));
    let (value, replayed) = recorded(&command(&tagged));
    assert!(replayed);
    assert_eq!(value, record(&["evidence", "release"]));
    assert_eq!(
        rejected(&command(&save("key-2", None, None))),
        (
            &Rejection::IdempotencyConflict,
            &RetryAdvice::RefreshAndNewRequest
        )
    );
    assert_eq!((handler.calls.get(), h.store.receipt_count()), (2, 2));

    // A receipt under a revision the operation does not register is a visible hold.
    h.store.rebind_receipts(7);
    assert_eq!(
        rejected(&command(&tagged)),
        (&Rejection::UnsupportedSchema, &RetryAdvice::ResolveReceipt)
    );
    assert_eq!(handler.calls.get(), 2);
}

fn without_tags(mut v: Value) -> Value {
    v.as_object_mut().unwrap().remove("tags");
    v
}

#[test]
fn query_results_are_published_under_the_negotiated_projection() {
    let h = Harness::new();
    let handler = Queries::new(|q| {
        // The use case sees the negotiated revision but may always produce the newest shape.
        assert!(q.schema_revision().0 >= 1);
        Ok(QueryAnswer {
            stamp: stamp(),
            result: result(
                "artifacts.list",
                json!({"items": [record(&["evidence"]), record(&[])], "hasMore": false}),
            ),
        })
    });
    let items = |frame: String| match h
        .gateway()
        .query(
            &context("user-a"),
            "artifacts.list",
            frame.as_bytes(),
            &handler,
        )
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => {
            serde_json::to_value(result).unwrap()["value"]["items"].clone()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        items(list(None, None)),
        json!([without_tags(record(&[])), without_tags(record(&[]))])
    );
    assert_eq!(
        items(list(Some(2), Some(json!(["evidence"])))),
        json!([record(&["evidence"]), record(&[])])
    );
}

/// SHA-256 of every revision-1 schema document as recorded before schema revisions 2 and 3 of
/// the history, session and artifact operations were added.
const RECORDED_REVISION_1: &[(&str, &str)] = &[
    (
        "addon-command-reply.schema.json",
        "637eee3780cc14573e10702b4aadbc09db930094e920f21e4aebf3380952085a",
    ),
    (
        "addon-command-request.schema.json",
        "76bba13007c24561a3ce613e4df1f8a943f9610203e5a7d256488fc8511572b2",
    ),
    (
        "artifact-command-reply.schema.json",
        "b206db3d40608fdc2e641e69504865691d273ba03c9206413b6b61931ba96f2c",
    ),
    (
        "artifact-command-request.schema.json",
        "525b9cc1962457686ca410addf6fecfe2dd0e1366ebbc9b574700bbcc5343848",
    ),
    (
        "artifact-deletion-request.schema.json",
        "cd68c9debdab547eeed3998d3929a939d4e22a8b2e44d2ef01ce741a3ea3c312",
    ),
    (
        "artifact-query-request.schema.json",
        "b3b808feaa9f97f6f243c6c222e455bff5af1998368c8de96d9c13f27db8da5c",
    ),
    (
        "attempt-envelope.schema.json",
        "fc2fa155875ecc3910b6d98e1b78184b79299b7987f713e9a7b8bfe158d948b7",
    ),
    (
        "capability-snapshot.schema.json",
        "311cff64bfa457b12d1d13671d66528e5176823e012dc3d9bf6d000516a4d649",
    ),
    (
        "command-reply.schema.json",
        "c718d37915e0029d7c98e1b33980e633f9d35923f98ab470be9fc9cd3ba84627",
    ),
    (
        "command-request.schema.json",
        "8e821e7c0f306e8870a698537f1cb86b1108679ac07d442650a7e818e67f880f",
    ),
    (
        "delivery-barrier.schema.json",
        "25aaa75a46cef7a0e3d055b8dff1a76fc25325f0a2efe5202abd80e3c28a72c2",
    ),
    (
        "delivery-snapshot.schema.json",
        "a5b59fd9d7dbed631c1445dbbda361b53a7c799f98717b9f9d131d6fee42806b",
    ),
    (
        "domain-command-reply.schema.json",
        "1c88a6a14db2850905c0265806823d7bdfa37a8923e824d3987f3bea19e95f2f",
    ),
    (
        "domain-query-reply.schema.json",
        "861ee54dfdbe7c693fe50747059b9f09fb79d954c6b19b14f49b326943fd8863",
    ),
    (
        "domain-request.schema.json",
        "ee593a4da87d9829dbe8ad9f5df109af1533dca52f839c5a06e40129c85ec2ee",
    ),
    (
        "event.schema.json",
        "89aa91f43de3328aba8af7fab44b964c541d04d19445c09be1f5cad64ebd85bc",
    ),
    (
        "git-command-request.schema.json",
        "a1718e29fbd02c1c30c9ccd8155f2f98fedb60cb3a00b299d5b08d27fb14fa74",
    ),
    (
        "graph-page.schema.json",
        "aca9af7d0e3c8615e00434eadc5556085108c08ee2a4edc2c8279a14c6439231",
    ),
    (
        "graph-read-request.schema.json",
        "1eece9e6fd9260c3af95fe787623839c7bfa72ade43ab97bf7337121b5bf133c",
    ),
    (
        "index-command-request.schema.json",
        "eb2bfdf772de716567bb0a6aaf11a1e0810a9cefa2d147a44b461d5045092845",
    ),
    (
        "inference-request.schema.json",
        "303b3a3d4bd49a39c44148658c2bc6320a4e77453960b75f07cfeb2051da8211",
    ),
    (
        "inference-spec.schema.json",
        "e4734b6a32933313adcdf74f07ada57c2a20f7d911eb676c7c63d44f1225d516",
    ),
    (
        "operation-completion.schema.json",
        "60ef6bd81f2d33e77cd2282fffce205f88982db13d35392f759df084b35fe7e1",
    ),
    (
        "operation-result.schema.json",
        "f50c651dcd949f4c3e2d8184957ca334b8c30830c0f221cc179731e6abce3316",
    ),
    (
        "package-host-request.schema.json",
        "190beae90c974720523774ca4cb366033ba74ba104dc46dbcdd780b30811f5b9",
    ),
    (
        "package-manifest.schema.json",
        "7f5989387bc31c599cb97ffe0b43dc52227aa2e36c075c54be55e69d6edd8b21",
    ),
    (
        "package-notification.schema.json",
        "d19320d2683b3bb226c9e65c88c83842d60702c8d6de6d4df889b05aa0d29a9d",
    ),
    (
        "package-request.schema.json",
        "44dcdc0367afa44757108bf0bb43dd39f9a1011739ed16150fce6e5669422ceb",
    ),
    (
        "package-response.schema.json",
        "303c7c0031c60eb1bf77be6d2aadbcfc1d09bfd51163e29c7b4646cc8555f517",
    ),
    (
        "provider-codec-event.schema.json",
        "16d1f63f42babd80f5840c16644706abd37a74b4c7ac9859463b86a8f06b31f9",
    ),
    (
        "provider-event.schema.json",
        "de1ac95963613ec2144471a56c55f6b88d1f1091a58e1839e153b07065c2a9c8",
    ),
    (
        "publication-request.schema.json",
        "416f189e19c7926c05f6fe4d979478036ce586336e59550b2577629e2944008c",
    ),
    (
        "query-reply.schema.json",
        "5e62392bbd693563a8dc1de57fe40e9a3025ae9d5cde4b3fe87c438a05ec802f",
    ),
    (
        "query-request.schema.json",
        "737647f75b83d06f768f00d5788e71ea7bf99b9c153dc9fed43c99ac20adbeb3",
    ),
    (
        "release-command-request.schema.json",
        "1f05af1440d9dc931c0cc26281b09f7616001a0d626f950265e71ee51402f815",
    ),
    (
        "resolved-view-set.schema.json",
        "38585a4d25f0dfdc6d03012c2d6f0e048c3f88729712dd63233b67ca1796d17e",
    ),
    (
        "retrieval-page.schema.json",
        "c54381130a251caccfcdd675820894a03073cf72915e2bff330b82f64c85b4da",
    ),
    (
        "retrieval-request.schema.json",
        "2bfc28986437a48a9894010a06f8c67490de27b260c45f19821d00051d36af7e",
    ),
    (
        "route-decision.schema.json",
        "3cbba022c46345fc38a9e64ffdc14cafb85358dfbbefd7a60eb91a4d9e22e38f",
    ),
    (
        "session-command-reply.schema.json",
        "a1db7ae3130a65b45c457687039220fd016677acedfb52ab5852f0045c2846da",
    ),
    (
        "session-command-request.schema.json",
        "2a9596d63653c6ddae06da13e17592a0c95104eaced405ef295a2710467a5f76",
    ),
    (
        "session-query-request.schema.json",
        "b1759a8b672311b7f705def14ebb8d4a209a24f56c142fa5f4ed2999034e032d",
    ),
    (
        "transcript-batch.schema.json",
        "fd9643a8069ac9f9098902cc41a4be1ae7e4463c92a26be523ced0a80eb65d1a",
    ),
    (
        "transcript-page.schema.json",
        "a8d9783c980489d9121986f906766bcd8492537c78670ecf7e34b5531fb143e4",
    ),
];
/// The hardened `RelativePath` pattern and the one it replaced.
const RELATIVE_PATH: &str = r"^(?![A-Za-z]:)(?:(?!\.{1,2}(?:[/\\]|$))[^/\\\u0000]+)(?:[/\\](?:(?!\.{1,2}(?:[/\\]|$))[^/\\\u0000]+))*$";
const RECORDED_RELATIVE_PATH: &str =
    r"^(?:(?!\.{1,2}(?:/|$))[^/\u0000]+)(?:/(?:(?!\.{1,2}(?:/|$))[^/\u0000]+))*$";

/// Removes the two documented exceptions from a revision-1 document: the branches of the new
/// `artifacts.tag` operation (with the definitions only they use) and the hardened
/// `RelativePath` pattern (a fix applied at every revision).
fn without_exceptions(mut doc: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for key in ["oneOf", "anyOf"] {
                    if let Some(Value::Array(alts)) = m.get_mut(key) {
                        alts.retain(|a| {
                            a.pointer("/properties/kind/const") != Some(&json!("artifacts.tag"))
                        });
                    }
                }
                if m.get("pattern") == Some(&json!(RELATIVE_PATH)) {
                    m.insert("pattern".into(), json!(RECORDED_RELATIVE_PATH));
                }
                // `host.activate` delivers the committed configuration (optional member; DW-43).
                if let Some(Value::Object(activate)) = m.get_mut("Activate")
                    && let Some(Value::Object(props)) = activate.get_mut("properties")
                {
                    props.remove("configuration");
                }
                m.values_mut().for_each(walk);
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    fn refs(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(m) => {
                if let Some(r) = m
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                {
                    out.insert(r.to_owned());
                }
                m.values().for_each(|x| refs(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }
    walk(&mut doc);
    if let Some(Value::Object(defs)) = doc.as_object_mut().and_then(|m| m.remove("$defs")) {
        let mut live = BTreeSet::new();
        refs(&doc, &mut live);
        let mut frontier: Vec<String> = live.iter().cloned().collect();
        while let Some(n) = frontier.pop() {
            let mut next = BTreeSet::new();
            if let Some(d) = defs.get(&n) {
                refs(d, &mut next);
            }
            frontier.extend(next.into_iter().filter(|x| live.insert(x.clone())));
        }
        let kept: serde_json::Map<String, Value> =
            defs.into_iter().filter(|(k, _)| live.contains(k)).collect();
        doc["$defs"] = Value::Object(kept);
    }
    doc
}

#[test]
fn revision_1_documents_are_byte_identical_to_the_recorded_ones_but_the_documented_exceptions() {
    let dir = common::contracts_dir();
    let mut exceptions = Vec::new();
    for (file, recorded) in RECORDED_REVISION_1 {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        if devworks_core::fingerprint::sha256_hex(text.as_bytes()) == *recorded {
            continue;
        }
        // Only the documented exceptions may differ, and nothing else.
        let normalized =
            serde_json::to_string_pretty(&without_exceptions(serde_json::from_str(&text).unwrap()))
                .unwrap()
                + "\n";
        assert_eq!(
            devworks_core::fingerprint::sha256_hex(normalized.as_bytes()),
            *recorded,
            "{file}: a revision-1 document changed beyond the documented exceptions"
        );
        exceptions.push(*file);
    }
    // The documents carrying the new operation, a relative path or the activation's
    // configuration.
    assert_eq!(
        exceptions,
        [
            "domain-command-reply.schema.json",
            "domain-query-reply.schema.json",
            "domain-request.schema.json",
            "operation-completion.schema.json",
            "operation-result.schema.json",
            "package-host-request.schema.json",
        ]
    );
}

#[test]
fn relative_paths_refuse_every_platform_absolute_or_escaping_form() {
    use devworks_core::protocol::support::{PathSet, RelativePath};
    let path = |s: &str| serde_json::from_value::<RelativePath>(json!(s));
    for bad in [
        "",
        "/etc/passwd",
        "\\root",
        "\\\\server\\share",
        "C:",
        "C:x",
        "C:/Windows",
        "c:\\Windows",
        "..",
        "../up",
        "..\\up",
        "src/../secrets",
        "src\\..\\secrets",
        "a/./b",
        "a\\.\\b",
        "a//b",
        "a\\\\b",
        "a/\\b",
        "a/",
        "a\\",
        "nul\u{0}byte",
    ] {
        assert!(path(bad).is_err(), "{bad:?} must be refused");
    }
    for good in [
        "src/lib.rs",
        "src/name:part\\literal.rs",
        "docs/a..b",
        "x.",
        ".hidden",
        "1:2",
    ] {
        assert!(path(good).is_ok(), "{good:?} must be accepted");
    }
    let set = |v: Value| serde_json::from_value::<PathSet>(v);
    assert!(set(json!(["a", "b"])).is_ok());
    assert!(set(json!(["a", "a"])).is_err() && set(json!([])).is_err());
    let schema = serde_json::to_value(schemars::schema_for!(PathSet)).unwrap();
    assert_eq!(schema["uniqueItems"], json!(true));
}
