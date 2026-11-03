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
