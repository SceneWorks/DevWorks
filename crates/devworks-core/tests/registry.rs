//! The generated registry must cover the complete recorded catalog exactly.
mod common;

use devworks_core::registry::{self, AuthorityClass, EffectClass, Envelope, Mode, SchemaRevision};
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

    assert_eq!(registry::all().len(), 369);
    assert_eq!(registry::all().len(), planned.len());
    assert_eq!(
        registry::all().len() as u64,
        register["operationCount"].as_u64().unwrap()
    );
    assert_eq!(plan["counts"]["operations"].as_u64().unwrap(), 369);
    assert_eq!(
        registry::families().len() as u64,
        register["familyCount"].as_u64().unwrap()
    );
    assert_eq!(registry::families().len(), 49);

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
        let result = serde_json::to_value(d.result_schema(d.revisions[0])).unwrap();
        assert!(result.is_object(), "{}", d.name);
        for b in d.modes {
            let schema = serde_json::to_value(b.input_schema(d.revisions[0])).unwrap();
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
    assert_eq!(count, 374);
}

/// Every property name declared anywhere in a schema, sorted (the envelope's own negotiated
/// `schemaRevision` member is not part of the typed input).
fn property_names(v: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::Object(props)) = m.get("properties") {
                    out.extend(props.keys().filter(|k| *k != "schemaRevision").cloned());
                }
                m.values().for_each(|x| walk(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out.sort();
    out
}

#[test]
fn schema_revisions_match_the_register_and_each_revision_has_its_own_schemas() {
    let register = json("docs/architecture/contracts/family-coverage.json");
    let mut revisioned = BTreeSet::new();
    let mut documents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut members = |file: &str| -> Vec<String> {
        documents
            .entry(file.to_owned())
            .or_insert_with(|| {
                property_names(&json(&format!("docs/architecture/contracts/{file}")))
            })
            .clone()
    };
    for o in register["families"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| f["operations"].as_array().unwrap())
    {
        let d = registry::find(o["operation"].as_str().unwrap()).unwrap();
        let listed: Vec<u32> = o["schemaRevisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["revision"].as_u64().unwrap() as u32)
            .collect();
        assert_eq!(
            d.revisions.iter().map(|r| r.0).collect::<Vec<_>>(),
            listed,
            "{} revisions",
            d.name
        );
        assert_eq!(
            d.revisions[0],
            SchemaRevision(
                o.get("introducedRevision")
                    .and_then(Value::as_u64)
                    .unwrap_or(1) as u32
            ),
            "{}",
            d.name
        );
        if d.revisions.len() > 1 {
            revisioned.insert(d.name);
        }
        // The native schema of every revision has the members of that revision's recorded
        // documents: specialized envelopes compare the whole envelope, every result its value.
        for r in o["schemaRevisions"].as_array().unwrap() {
            let revision = SchemaRevision(r["revision"].as_u64().unwrap() as u32);
            if let Envelope::Specialized { .. } = d.envelope {
                let native = serde_json::to_value(d.modes[0].input_schema(revision)).unwrap();
                assert_eq!(
                    property_names(&native),
                    members(r["inputSchema"].as_str().unwrap()),
                    "{} input at revision {}",
                    d.name,
                    revision.0
                );
            }
            if d.revisions.len() == 1 {
                continue;
            }
            let document = members(r["resultSchema"].as_str().unwrap());
            let native = serde_json::to_value(d.result_schema(revision)).unwrap();
            for name in property_names(&native) {
                assert!(
                    document.binary_search(&name).is_ok(),
                    "{} result member {name} at revision {} is not in its document",
                    d.name,
                    revision.0
                );
            }
        }
    }
    assert_eq!(
        revisioned,
        BTreeSet::from([
            "addons.catalog",
            "addons.configure",
            "addons.disable",
            "addons.enable",
            "addons.install",
            "addons.rollback",
            "addons.uninstall",
            "addons.update",
            "desktop.capture",
            "desktop.input",
            "desktop.inspect",
            "desktop.release_control",
            "desktop.request_control",
            "approvals.list",
            "approvals.inspect",
            "approvals.propose",
            "approvals.record_human_decision",
            "notifications.list",
            "notifications.read",
            "notifications.acknowledge",
            "router.usage",
            "router.price_status",
            "router.price_override",
            "help.search",
            "help.read",
            "help.topic_for_screen",
            "help.package_guides",
            "conventions.applicability",
            "conventions.delete",
            "conventions.delete_resource",
            "conventions.edit",
            "conventions.import",
            "conventions.index",
            "conventions.list",
            "conventions.move",
            "conventions.read",
            "conventions.resources",
            "conventions.retire",
            "conventions.search",
            "conventions.version",
            "repositories.nodes",
            "code.search",
            "code.architecture",
            "code.dependency",
            "code.impact",
            "code.source",
            "code.traverse",
            "code.graph_scope_status",
            "code.overview",
            "code.nodes_by_file",
            "code.node_detail",
            "code.set_trust",
            "code.file_impact",
            "code.graph_schema",
            "code.call_path",
            "code.data_lineage",
            "code.consumers",
            "code.publishers",
            "code.snippet",
            "code.repository_dependencies",
            "artifacts.list",
            "artifacts.read",
            "artifacts.save",
            "browser.console",
            "browser.input",
            "browser.inspect",
            "browser.navigate",
            "browser.network",
            "browser.screenshot",
            "browser.sessions",
            "browser.upload",
            "dreaming.revert",
            "git.pull_request",
            "history.command_receipt",
            "history.events",
            "history.evidence_links",
            "history.git_links",
            "history.tool_usage",
            "history.work_lineage",
            "indexing.refresh_overlay",
            "indexing.runs",
            "indexing.status",
            "knowledge.challenge",
            "knowledge.delete_by_source",
            "knowledge.claim_bundle",
            "knowledge.diagnostics",
            "knowledge.entity_bundle",
            "knowledge.entity_graph",
            "knowledge.expand_frontier",
            "knowledge.import_user_data",
            "knowledge.promote",
            "knowledge.query",
            "knowledge.read",
            "knowledge.record",
            "knowledge.relate",
            "knowledge.retract",
            "knowledge.revise",
            "knowledge.scope",
            "knowledge.search",
            "knowledge.subgraph",
            "knowledge.tag",
            "knowledge.tagged_lessons",
            "knowledge.verify",
            "knowledge.write_status",
            "mcp.capabilities",
            "mcp.configure",
            "mcp.connect",
            "mcp.discover",
            "mcp.prompts",
            "mcp.resources",
            "mcp.select_tools",
            "mcp.servers",
            "jobs.cancel",
            "jobs.create",
            "jobs.delete",
            "jobs.disable",
            "jobs.edit",
            "jobs.enable",
            "jobs.inspect",
            "jobs.list",
            "jobs.run_now",
            "resources.acquire",
            "resources.holders",
            "resources.list",
            "resources.release",
            "resources.renew",
            "resources.waiters",
            "projects.create",
            "workflows.list",
            "workflows.create_status",
            "workflows.rename_status",
            "workflows.order",
            "next_work.claim_project_queue",
            "next_work.claim_feature_task",
            "next_work.inspect_claim",
            "next_work.release_claim",
            "router.availability",
            "router.catalog",
            "router.pin",
            "router.route",
            "router.configure",
            "router.connections",
            "schemas.configure",
            "schemas.connections",
            "schemas.index",
            "schemas.objects",
            "schemas.refresh",
            "schemas.relationships",
            "schemas.remove_index",
            "database.configure_sensitive_columns",
            "sessions.append_message",
            "sessions.briefing",
            "sessions.end",
            "sessions.get",
            "sessions.list",
            "sessions.start",
            "web.fetch",
            "web.search",
            "workspace.diagnostics",
            "workspace.logs",
            "workspace.record_diagnostic",
            // Feature readiness (revision 2 of every operation returning a work record).
            "features.list",
            "features.get",
            "features.create",
            "features.edit_plan",
            "features.dependencies",
            "features.edit",
            "issues.list",
            "issues.get",
            "issues.create",
            "issues.edit",
            "tasks.list",
            "tasks.get",
            "tasks.create",
            "tasks.edit",
            "tasks.move",
            "tasks.assign",
            "tasks.dependency_links",
            "tasks.acceptance_evidence",
            "tasks.sync_plan",
            "tasks.set_feature",
            // A prioritization's bounded placement.
            "features.prioritize",
            "issues.prioritize",
            "tasks.prioritize",
        ])
    );
    // Revision 1 of the history pages is exactly the recorded Page; revision 2 adds the tail
    // checkpoint, the command aggregate, structured attribution and tool invocations.
    let schema = |s| serde_json::to_value(s).unwrap().to_string();
    for (name, members) in [
        ("history.events", &["checkpoint", "command"][..]),
        (
            "history.work_lineage",
            &["checkpoint", "actorId", "occurredAtMs", "repositoryId"][..],
        ),
        ("history.git_links", &["checkpoint", "actorId"][..]),
        ("history.evidence_links", &["checkpoint", "actorId"][..]),
        ("history.tool_usage", &["checkpoint", "toolInvocations"][..]),
    ] {
        let d = registry::find(name).unwrap();
        let (r1, r2) = (
            schema(d.result_schema(SchemaRevision(1))),
            schema(d.result_schema(SchemaRevision(2))),
        );
        for m in members {
            let quoted = format!("\"{}\"", m.trim_matches('"'));
            assert!(!r1.contains(&quoted), "{name} revision 1 has {m}");
            assert!(r2.contains(&quoted), "{name} revision 2 lacks {m}");
        }
        assert!(
            !r1.contains("x-since") && r2.contains("\"x-since\":2"),
            "{name}"
        );
    }
    let events = registry::find("history.events").unwrap();
    let (q1, q2) = (
        schema(events.modes[0].input_schema(SchemaRevision(1))),
        schema(events.modes[0].input_schema(SchemaRevision(2))),
    );
    assert!(!q1.contains("\"projectId\"") && q2.contains("\"projectId\""));
    let briefing = registry::find("sessions.briefing").unwrap();
    let (b1, b2) = (
        schema(briefing.modes[0].input_schema(SchemaRevision(1))),
        schema(briefing.modes[0].input_schema(SchemaRevision(2))),
    );
    for m in ["item", "paths", "budgetTokens"] {
        assert!(!b1.contains(&format!("\"{m}\"")) && b2.contains(&format!("\"{m}\"")));
    }
    // Revision 1 is exactly the earlier shape; revision 2 adds the tags member and its bounds.
    for (name, mode) in [
        ("artifacts.save", Mode::Command),
        ("artifacts.list", Mode::Query),
    ] {
        let d = registry::find(name).unwrap();
        let schema = |s| serde_json::to_value(s).unwrap().to_string();
        let (input1, input2) = (
            schema(d.binding(mode).unwrap().input_schema(SchemaRevision(1))),
            schema(d.binding(mode).unwrap().input_schema(SchemaRevision(2))),
        );
        let (result1, result2) = (
            schema(d.result_schema(SchemaRevision(1))),
            schema(d.result_schema(SchemaRevision(2))),
        );
        for s in [&input1, &result1] {
            assert!(
                !s.contains("\"tags\"") && !s.contains("ArtifactTags"),
                "{name}"
            );
        }
        for s in [&input2, &result2] {
            assert!(
                s.contains("\"tags\"") && s.contains("\"x-since\":2"),
                "{name}"
            );
            assert!(s.contains("\"maxItems\":32") && s.contains("\"uniqueItems\":true"));
        }
    }
}

#[test]
fn completion_binding_and_action_dependent_classification_match_the_register() {
    // Every command binds an asynchronous completion contract in the register; no query does.
    for d in registry::all() {
        assert_eq!(
            d.completion,
            d.binding(Mode::Command).is_some(),
            "{}",
            d.name
        );
    }
    // Exactly the two recorded action-dependent commands exist, and their recorded fixtures
    // classify to a concrete effect before authority (AD69).
    let dependent: Vec<_> = registry::all()
        .iter()
        .filter(|d| {
            d.modes
                .iter()
                .any(|b| b.effect == EffectClass::ActionDependent)
        })
        .map(|d| d.name)
        .collect();
    assert_eq!(dependent, ["github.account", "browser.sessions"]);
    let fixtures = json("docs/architecture/contracts/domain-fixtures.json");
    for name in dependent {
        let raw = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == format!("input-{name}-Command"))
            .and_then(|f| f["raw"].as_str())
            .expect("command fixture");
        let decoded = devworks_core::admission::decode(
            devworks_core::admission::Selection {
                operation: name,
                mode: Mode::Command,
            },
            raw.as_bytes(),
        )
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_ne!(decoded.effect, EffectClass::ActionDependent, "{name}");
        assert!(decoded.action.is_some(), "{name}");
    }
}
