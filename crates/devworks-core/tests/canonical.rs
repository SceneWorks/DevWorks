//! Native RFC 8785 canonicalization and fingerprint qualification.
//!
//! Number vectors are RFC 8785 Appendix B; structural vectors are its sections 3.2.2/3.2.3.
//! "Authoring" known answers were produced once with the `rfc8785` 0.1.4 Python library and
//! `hashlib.sha256` (the same authoring library the recorded probe evidence used) and are pinned
//! here so the native implementation is cross-checked against an independent one.
mod common;

use devworks_core::admission::{self, Selection};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::jcs::{self, JcsError};
use devworks_core::registry::Mode;
use serde_json::{Value, json};

fn canonical(text: &str) -> String {
    jcs::canonicalize(&serde_json::from_str::<Value>(text).unwrap()).unwrap()
}

#[test]
fn rfc8785_appendix_b_number_serialization() {
    let vectors: [(u64, &str); 21] = [
        (0x0000000000000000, "0"),
        (0x8000000000000000, "0"),
        (0x0000000000000001, "5e-324"),
        (0x8000000000000001, "-5e-324"),
        (0x7fefffffffffffff, "1.7976931348623157e+308"),
        (0xffefffffffffffff, "-1.7976931348623157e+308"),
        (0x4340000000000000, "9007199254740992"),
        (0xc340000000000000, "-9007199254740992"),
        (0x4430000000000000, "295147905179352830000"),
        (0x44b52d02c7e14af5, "9.999999999999997e+22"),
        (0x44b52d02c7e14af6, "1e+23"),
        (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
        (0x444b1ae4d6e2ef4e, "999999999999999700000"),
        (0x444b1ae4d6e2ef4f, "999999999999999900000"),
        (0x444b1ae4d6e2ef50, "1e+21"),
        (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
        (0x3eb0c6f7a0b5ed8d, "0.000001"),
        (0x41b3de4355555553, "333333333.3333332"),
        (0x41b3de4355555554, "333333333.33333325"),
        (0x41b3de4355555555, "333333333.3333333"),
        (0x43143ff3c1cb0959, "1424953923781206.2"),
    ];
    for (bits, expected) in vectors {
        assert_eq!(
            jcs::ecmascript_number(f64::from_bits(bits)),
            expected,
            "{bits:#018x}"
        );
    }
    assert_eq!(
        jcs::ecmascript_number(f64::from_bits(0x41b3de4355555556)),
        "333333333.3333334"
    );
    assert_eq!(
        jcs::ecmascript_number(f64::from_bits(0x41b3de4355555557)),
        "333333333.33333343"
    );
    assert_eq!(
        jcs::ecmascript_number(f64::from_bits(0xbecbf647612f3696)),
        "-0.0000033333333333333333"
    );
}

#[test]
fn rfc8785_structural_examples() {
    let input = r#"{
        "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
        "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
        "literals": [null, true, false]
    }"#;
    assert_eq!(
        canonical(input),
        "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}"
    );
    let sorting = r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\ufb33":"Hebrew Letter Dalet With Dagesh","1":"One","\ud83d\ude00":"Emoji: Grinning Face","\u0080":"Control","\u00f6":"Latin Small Letter O With Diaeresis"}"#;
    assert_eq!(
        canonical(sorting),
        "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\"ö\":\"Latin Small Letter O With Diaeresis\",\"€\":\"Euro Sign\",\"😀\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}"
    );
}

#[test]
fn authoring_library_known_answer_for_mixed_document() {
    let mixed = json!({
        "z": [1, 2.5, -0.0, 1e21, 1e-7, 9007199254740991_i64, -9007199254740991_i64, 0.1, 1.0000000000000002, 5e-324],
        "a\u{e9}": "line\nbreak\t\u{1}\u{7f}\u{2028}",
        "nested": {"b": null, "a": true}
    });
    let text = jcs::canonicalize(&mixed).unwrap();
    assert_eq!(
        text,
        "{\"a\u{e9}\":\"line\\nbreak\\t\\u0001\u{7f}\u{2028}\",\"nested\":{\"a\":true,\"b\":null},\"z\":[1,2.5,0,1e+21,1e-7,9007199254740991,-9007199254740991,0.1,1.0000000000000002,5e-324]}"
    );
    assert_eq!(
        sha256_hex(text.as_bytes()),
        "e97018fb5470147e5e43f5e6fd0998186598abf507a5fe030b21f6dcc7cbb421"
    );
}

#[test]
fn integers_outside_the_safe_range_are_refused_not_rounded() {
    assert!(jcs::canonicalize(&json!(9007199254740991_i64)).is_ok());
    for n in [9007199254740992_i64, -9007199254740992, i64::MAX] {
        assert!(
            matches!(
                jcs::canonicalize(&json!(n)),
                Err(JcsError::NonInteroperableNumber(_))
            ),
            "{n}"
        );
    }
    assert!(jcs::canonicalize(&json!(u64::MAX)).is_err());
}

fn fingerprint(operation: &str, mode: Mode, raw: &str) -> String {
    admission::decode(Selection { operation, mode }, raw.as_bytes())
        .unwrap_or_else(|r| panic!("{operation}: {}", r.error))
        .fingerprint
        .to_string()
}

const START: &str = r#"{"protocol":"1.0-draft.1","requestKey":"key-start","command":{"kind":"runs.start","rootId":"root-a","expectedWorkRevision":"1","expectedControlRevision":"1"}}"#;

#[test]
fn native_fingerprints_match_authoring_library_known_answers() {
    assert_eq!(
        fingerprint("runs.start", Mode::Command, START),
        "b0944a8d96d87e894c041b08ffbfd4d21c83a5de42c0f7bf856aebab6005ab22"
    );
    let cont = r#"{"protocol":"1.0-draft.1","requestKey":"key-continue","command":{"kind":"runs.continue","rootId":"root-a","expectedControlRevision":"1","coveredHolds":[{"holdId":"hold-a","revision":"1"}]}}"#;
    assert_eq!(
        fingerprint("runs.continue", Mode::Command, cont),
        "2f05cf20c31356ae8ed8ef726dd7b796480e471c2ea1f965dbe967cecd12e860"
    );
    let edit = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"request-a","body":{"kind":"projects.edit","projectId":"project-a","expectedRevision":"1","changes":{}}}"#;
    assert_eq!(
        fingerprint("projects.edit", Mode::Command, edit),
        "e0b0b238c91eaa9cc4f7beb78592c2ad29f8ad1e7d40e883acda59118f8a16c2"
    );
}

#[test]
fn fingerprint_ignores_layout_and_request_key_but_not_intent() {
    let base = fingerprint("runs.start", Mode::Command, START);
    let reordered = r#"{ "command": { "expectedControlRevision": "1", "rootId": "root-a",
        "expectedWorkRevision": "1", "kind": "runs.start" }, "requestKey": "another-key", "protocol": "1.0-draft.1" }"#;
    assert_eq!(fingerprint("runs.start", Mode::Command, reordered), base);
    let changed_revision = START.replace(
        "\"expectedControlRevision\":\"1\"",
        "\"expectedControlRevision\":\"2\"",
    );
    assert_ne!(
        fingerprint("runs.start", Mode::Command, &changed_revision),
        base
    );
    let changed_root = START.replace("root-a", "root-b");
    assert_ne!(
        fingerprint("runs.start", Mode::Command, &changed_root),
        base
    );
}

#[test]
fn covered_hold_set_is_normalized_and_explicit_null_equals_absence() {
    let holds = |order: &str| {
        format!(
            r#"{{"protocol":"1.0-draft.1","requestKey":"k","command":{{"kind":"runs.continue","rootId":"root-a","expectedControlRevision":"1","coveredHolds":{order}}}}}"#
        )
    };
    let az = holds(r#"[{"holdId":"hold-a","revision":"1"},{"holdId":"hold-z","revision":"2"}]"#);
    let za = holds(r#"[{"holdId":"hold-z","revision":"2"},{"holdId":"hold-a","revision":"1"}]"#);
    assert_eq!(
        fingerprint("runs.continue", Mode::Command, &az),
        fingerprint("runs.continue", Mode::Command, &za)
    );

    let list = |extra: &str| {
        format!(
            r#"{{"mode":"Query","protocol":"1.0-draft.1","queryId":"q","body":{{"kind":"projects.list","scope":{{"kind":"Workspace"}},"page":{{"limit":20}}{extra}}}}}"#
        )
    };
    assert_eq!(
        fingerprint("projects.list", Mode::Query, &list("")),
        fingerprint("projects.list", Mode::Query, &list(r#","lifecycle":null"#))
    );
}

#[test]
fn ordered_lists_and_user_text_are_not_normalized() {
    let order = |ids: &str| {
        format!(
            r#"{{"mode":"Command","protocol":"1.0-draft.1","requestKey":"k","body":{{"kind":"workflows.order","projectId":"project-a","expectedWorkflowRevision":"1","orderedStatusIds":{ids}}}}}"#
        )
    };
    assert_ne!(
        fingerprint("workflows.order", Mode::Command, &order(r#"["s-a","s-b"]"#)),
        fingerprint("workflows.order", Mode::Command, &order(r#"["s-b","s-a"]"#))
    );
    let rename = |label: &str| {
        format!(
            r#"{{"mode":"Command","protocol":"1.0-draft.1","requestKey":"k","body":{{"kind":"workflows.rename_status","projectId":"project-a","statusId":"id-a","expectedRevision":"1","label":"{label}"}}}}"#
        )
    };
    // NFC "é" and NFD "e\u0301" are different user text.
    assert_ne!(
        fingerprint(
            "workflows.rename_status",
            Mode::Command,
            &rename("caf\u{e9}")
        ),
        fingerprint(
            "workflows.rename_status",
            Mode::Command,
            &rename("cafe\u{301}")
        )
    );
}
