//! The generated registry must cover the complete recorded catalog exactly.
mod common;

use devworks_core::registry::{self, AuthorityClass, EffectClass, Envelope, Mode};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn json(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(common::repo_file(path)).unwrap()).unwrap()
}
fn modes(v: &Value) -> Vec<Mode> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|m| match m.as_str().unwrap() {
            "Query" => Mode::Query,
            "Command" => Mode::Command,
            other => panic!("unknown mode {other}"),
        })
        .collect()
}

#[test]
fn registry_matches_implementation_plan_and_family_register() {
    let plan = json("docs/architecture/implementation-plan.json");
    let register = json("docs/architecture/contracts/family-coverage.json");
    let planned: BTreeMap<&str, &Value> = plan["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["operation"].as_str().unwrap(), o))
        .collect();

    assert_eq!(registry::all().len(), 353);
    assert_eq!(registry::all().len(), planned.len());
    assert_eq!(
        registry::all().len() as u64,
        register["operationCount"].as_u64().unwrap()
    );
    assert_eq!(plan["counts"]["operations"].as_u64().unwrap(), 353);
    assert_eq!(
        registry::families().len() as u64,
        register["familyCount"].as_u64().unwrap()
    );
    assert_eq!(registry::families().len(), 48);

    let registered: BTreeSet<&str> = registry::all().iter().map(|d| d.name).collect();
    assert_eq!(
        registered,
        planned.keys().copied().collect(),
        "operation names differ"
    );

    for d in registry::all() {
        let p = planned[d.name];
        assert_eq!(d.family, p["family"].as_str().unwrap(), "{} family", d.name);
        assert_eq!(
            d.owner,
            p["serviceOwner"].as_str().unwrap(),
            "{} owner",
            d.name
        );
        assert_eq!(
            d.modes.iter().map(|b| b.mode).collect::<Vec<_>>(),
            modes(&p["requestModes"]),
            "{} modes",
            d.name
        );
        assert_eq!(
            d.result_type,
            p["resultValueType"].as_str().unwrap(),
            "{} result",
            d.name
        );
        let schema = p["inputSchema"]
            .as_str()
            .unwrap()
            .trim_end_matches(".schema.json");
        match d.envelope {
            Envelope::Domain => {
                assert_eq!(schema, "domain-request", "{}", d.name);
                let sel = d.selector.expect("domain selector");
                assert_eq!((sel.pointer, sel.equals), ("/body/kind", d.name));
            }
            Envelope::Specialized { schema: s } => assert_eq!(s, schema, "{}", d.name),
        }
        assert!(registry::find(d.name).is_some_and(|f| std::ptr::eq(f, d)));
        for b in d.modes {
            // Read entitlements never carry a mutating effect.
            if b.mode == Mode::Query {
                assert_eq!(
                    (b.authority, b.effect),
                    (AuthorityClass::ScopedRead, EffectClass::ReadOnly),
                    "{}",
                    d.name
                );
            } else {
                assert_ne!(b.authority, AuthorityClass::ScopedRead, "{}", d.name);
            }
        }
    }
    for family in registry::families() {
        assert!(
            registry::all().iter().any(|d| d.family == *family),
            "{family} has no operations"
        );
    }
    assert!(registry::find("projects.unknown").is_none());
    assert!(registry::find("").is_none());
}

#[test]
fn exactly_the_five_recorded_hybrid_names_have_both_modes() {
    let hybrids: BTreeSet<&str> = registry::all()
        .iter()
        .filter(|d| d.modes.len() == 2)
        .map(|d| d.name)
        .collect();
    assert_eq!(
        hybrids,
        BTreeSet::from([
            "browser.sessions",
            "dreaming.instructions",
            "dreaming.settings",
            "github.account",
            "notifications.settings"
        ])
    );
    for name in &hybrids {
        let d = registry::find(name).unwrap();
        let (q, c) = (
            d.binding(Mode::Query).unwrap(),
            d.binding(Mode::Command).unwrap(),
        );
        assert_ne!(
            q.input_type, c.input_type,
            "{name}: read and command inputs must be distinct types"
        );
    }
}

#[test]
fn every_binding_publishes_strict_input_and_result_schemas() {
    let mut count = 0;
    for d in registry::all() {
        let result = serde_json::to_value(d.result_schema()).unwrap();
        assert!(result.is_object(), "{}", d.name);
        for b in d.modes {
            let schema = serde_json::to_value(b.input_schema()).unwrap();
            if d.envelope == Envelope::Domain {
                // Agent tool descriptors expose the body members; unknown members are refused.
                assert_eq!(
                    schema["additionalProperties"],
                    Value::Bool(false),
                    "{} input schema not closed",
                    d.name
                );
            }
            count += 1;
        }
    }
    assert_eq!(count, 358);
}
