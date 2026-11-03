//! Replays the recorded package/codec fixtures through the public wire DTOs and asserts the
//! recorded typed outcome. Duplicate members are rejected first, as the host framing does.
use devworks_extension_protocol::codec::ProviderCodecEvent;
use devworks_extension_protocol::package::*;
use devworks_extension_protocol::scalars::ArtifactRef;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    id: String,
    entry: String,
    raw: String,
    typed_valid: bool,
    #[allow(dead_code)]
    schema_valid: bool,
}

fn contracts(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/architecture/contracts")
        .join(file);
    std::fs::read_to_string(path).expect("fixture file")
}

/// Rejects duplicate object members at any depth (serde_json would otherwise keep the last one).
fn no_duplicates(raw: &str) -> Result<(), String> {
    struct Strict;
    impl<'de> Deserialize<'de> for Strict {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Strict)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Strict {
        type Value = Strict;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON value")
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Strict, E> {
            Ok(Strict)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Strict, E> {
            Ok(Strict)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Strict, E> {
            Ok(Strict)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Strict, E> {
            Ok(Strict)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Strict, E> {
            Ok(Strict)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Strict, E> {
            Ok(Strict)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
            while a.next_element::<Strict>()?.is_some() {}
            Ok(Strict)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
            let mut seen = std::collections::HashSet::new();
            while let Some(k) = a.next_key::<String>()? {
                if !seen.insert(k.clone()) {
                    return Err(serde::de::Error::custom(format!("duplicate member {k}")));
                }
                a.next_value::<Strict>()?;
            }
            Ok(Strict)
        }
    }
    serde_json::from_str::<Strict>(raw)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn dto<T: DeserializeOwned>(
    raw: &str,
    check: impl FnOnce(&T) -> Result<(), &'static str>,
) -> Result<(), String> {
    if raw.len() > 1024 * 1024 {
        return Err("frame too large".into());
    }
    no_duplicates(raw)?;
    let typed: T = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    check(&typed).map_err(str::to_owned)
}

fn check(case: &Fixture) -> Option<Result<(), String>> {
    let raw = case.raw.as_str();
    Some(match case.entry.as_str() {
        "package-manifest" => dto::<PackageManifest>(raw, |v| v.validate()),
        "package-host-request" => dto::<HostRequest>(raw, |v| v.validate()),
        "package-request" => dto::<PackageRequest>(raw, |v| v.validate()),
        "package-notification" => dto::<PackageNotification>(raw, |v| v.validate()),
        "package-response" => dto::<RpcResponse>(raw, |v| v.validate()),
        "provider-codec-event" => dto::<ProviderCodecEvent>(raw, |v| v.validate()),
        _ => return None,
    })
}

#[test]
fn package_and_codec_fixtures_match_recorded_outcomes() {
    let cases: Vec<Fixture> =
        serde_json::from_str(&contracts("provider-package-fixtures.json")).unwrap();
    assert_eq!(cases.len(), 120);
    let mut checked = BTreeMap::<&str, usize>::new();
    let mut failures = Vec::new();
    for case in &cases {
        let Some(outcome) = check(case) else { continue };
        *checked.entry(case.entry.as_str()).or_default() += 1;
        if outcome.is_ok() != case.typed_valid {
            failures.push(format!(
                "{} [{}] expected {} got {outcome:?}",
                case.id, case.entry, case.typed_valid
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // 66 public-protocol cases; the other 54 belong to devworks-core.
    assert_eq!(checked.values().sum::<usize>(), 66, "{checked:?}");
}

#[test]
fn recorded_helper_frames_decode_as_host_requests() {
    let frames: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&contracts("helper-fixture.json")).unwrap();
    for (name, frame) in &frames {
        let raw = frame.to_string();
        if name == "artifact" {
            dto::<ArtifactRef>(&raw, |v| v.validate()).unwrap();
        } else {
            dto::<HostRequest>(&raw, |v| v.validate()).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
    assert_eq!(frames.len(), 6);
}

#[test]
fn duplicate_members_are_refused() {
    let shutdown = r#"{"jsonrpc":"2.0","id":"s:1","method":"host.shutdown","params":{"processSessionId":"session-a","deadlineMs":1000}}"#;
    assert!(dto::<HostRequest>(shutdown, |v| v.validate()).is_ok());
    let duplicate = shutdown.replace(
        r#""deadlineMs":1000"#,
        r#""deadlineMs":1000,"deadlineMs":2000"#,
    );
    assert!(
        dto::<HostRequest>(&duplicate, |v| v.validate())
            .unwrap_err()
            .contains("duplicate member")
    );
}
