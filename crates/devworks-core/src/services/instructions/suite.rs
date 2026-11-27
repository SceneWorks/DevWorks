//! Eval suites (DW-72): versioned, file-backed measurement kept apart from what it measures.
//!
//! A suite is a folder in the shared eval root (`~/.agents/evals`, read in place and never
//! written) or the harness eval root (`<data-root>/evals`), holding
//!
//! * `SUITE.md`, its documentation: what it measures, each grader with what it checks and
//!   cannot check, how to run it and how to read its results ([`validate_documentation`]);
//! * `suite.json`, the subjects it measures and its deterministic checks ([`Definition`]);
//! * `exceptions.json`, the exceptions a person accepted, written only by
//!   `evals.accept_exception`; and
//! * resources under `cases/`, `graders/`, `rubrics/` and `fixtures/` (anything else is a
//!   document).
//!
//! It is an instruction item of kind `Suite`, so it has the same immutable revisions, observed
//! folder changes awaiting a person's confirmation, harness overrides of shared folders and
//! exact human-approved activation of agent drafts as skills. Every revision records its parsed
//! definition in its origin (`origin.suite`), so reads never re-parse files. A suite never
//! carries a skill, and a skill proposal never carries a suite, so one proposal cannot change
//! both a subject and its measurement.
use super::scan;
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{
    InstructionOrigin, InstructionRevisionState, StorageError, StoredInstructionItem,
    StoredInstructionRevision,
};
use crate::protocol::Id;
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{
    EvalAcceptedException, EvalDeterministicCheck, EvalSubjectRef, EvalSuiteOrigin, EvalSuitePart,
    EvalSuiteRecord, EvalSuiteResource, InstructionState, RegisteredPath,
};
use crate::protocol::support::{BoundedList, Check, RelativePath};
use crate::services::kernel::{Failure, label, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SUITE_FILE: &str = "SUITE.md";
pub const DEFINITION_FILE: &str = "suite.json";
pub const EXCEPTIONS_FILE: &str = "exceptions.json";
/// Registered root of the user-owned shared suites (`~/.agents/evals`).
pub const SHARED_ROOT_ID: &str = "evals-shared";
/// Registered root of the harness-owned suites (`<data-root>/evals`).
pub const HARNESS_ROOT_ID: &str = "evals-harness";
/// The sections every suite's documentation has, as second-level headings.
pub const SECTIONS: [&str; 4] = [
    "What it measures",
    "Graders",
    "How to run",
    "Reading results",
];
/// The most subjects, checks or exceptions one revision holds (the record's list bound).
pub const MAX_ENTRIES: usize = 128;
/// The most bytes `suite.json` or `exceptions.json` may hold.
pub const MAX_DEFINITION_BYTES: usize = 256 * 1024;

/// `suite.json`: what the suite measures and its deterministic checks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Definition {
    pub measures: Vec<EvalSubjectRef>,
    pub checks: Vec<EvalDeterministicCheck>,
}

/// A revision's validated contents, recorded in its origin as `suite`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Parsed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub measures: Vec<EvalSubjectRef>,
    pub checks: Vec<EvalDeterministicCheck>,
    pub exceptions: Vec<EvalAcceptedException>,
}

/// The canonical bytes of a `suite.json` (pretty JSON with a final newline).
pub fn definition_bytes(definition: &Definition) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(definition).expect("suite definition");
    bytes.push(b'\n');
    bytes
}
/// The canonical bytes of an `exceptions.json`.
pub fn exceptions_bytes(exceptions: &[EvalAcceptedException]) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(exceptions).expect("suite exceptions");
    bytes.push(b'\n');
    bytes
}

/// Whether `path` names a skill manifest (`SKILL.md` in any folder, in any case).
pub fn is_skill_manifest(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|f| f.eq_ignore_ascii_case(scan::SKILL_FILE))
}
/// Whether `path` names an eval suite's own file (`SUITE.md` or `suite.json` in any folder):
/// a skill revision carrying one bundles its own measurement.
pub fn is_suite_file(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|f| {
        f.eq_ignore_ascii_case(SUITE_FILE) || f.eq_ignore_ascii_case(DEFINITION_FILE)
    })
}

/// The part of a suite a resource path is.
pub fn part(path: &str) -> EvalSuitePart {
    match path.split('/').next().unwrap_or_default() {
        "cases" => EvalSuitePart::Case,
        "graders" => EvalSuitePart::Grader,
        "rubrics" => EvalSuitePart::Rubric,
        "fixtures" => EvalSuitePart::Fixture,
        _ => EvalSuitePart::Document,
    }
}

/// Validates a suite folder: its documentation and every file beside it, by path.
pub fn parse(documentation: &str, files: &[(&str, &[u8])]) -> Result<Parsed, String> {
    if let Some((path, _)) = files.iter().find(|(p, _)| is_skill_manifest(p)) {
        return Err(format!(
            "{path}: a suite carries no skill; keep the skill in its own folder"
        ));
    }
    let definition = files
        .iter()
        .find(|(p, _)| *p == DEFINITION_FILE)
        .ok_or_else(|| format!("{DEFINITION_FILE} is missing"))?
        .1;
    let definition: Definition = read_json(DEFINITION_FILE, definition)?;
    let exceptions: Vec<EvalAcceptedException> =
        match files.iter().find(|(p, _)| *p == EXCEPTIONS_FILE) {
            Some((_, bytes)) => read_json(EXCEPTIONS_FILE, bytes)?,
            None => Vec::new(),
        };
    check_definition(&definition)?;
    check_exceptions(&definition.checks, &exceptions)?;
    validate_documentation(documentation, &definition.checks)?;
    Ok(Parsed {
        description: scan::description(documentation).map(|d| d.chars().take(4096).collect()),
        measures: definition.measures,
        checks: definition.checks,
        exceptions,
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &str, bytes: &[u8]) -> Result<T, String> {
    if bytes.len() > MAX_DEFINITION_BYTES {
        return Err(format!("{path} exceeds {MAX_DEFINITION_BYTES} bytes"));
    }
    serde_json::from_slice(bytes).map_err(|e| format!("{path}: {e}"))
}

/// The subjects and checks are bounded, valid and named uniquely.
pub fn check_definition(definition: &Definition) -> Result<(), String> {
    if definition.measures.is_empty() || definition.measures.len() > MAX_ENTRIES {
        return Err(format!(
            "{DEFINITION_FILE}: measures names 1 to {MAX_ENTRIES} subjects"
        ));
    }
    if definition.checks.is_empty() || definition.checks.len() > MAX_ENTRIES {
        return Err(format!(
            "{DEFINITION_FILE}: checks holds 1 to {MAX_ENTRIES} checks"
        ));
    }
    for subject in &definition.measures {
        subject
            .check()
            .map_err(|e| format!("{DEFINITION_FILE}: subject: {e}"))?;
    }
    let mut ids = std::collections::BTreeSet::new();
    for check in &definition.checks {
        check
            .check()
            .map_err(|e| format!("{DEFINITION_FILE}: check {}: {e}", check.id.as_str()))?;
        if !ids.insert(check.id.as_str()) {
            return Err(format!(
                "{DEFINITION_FILE}: check {} is named twice",
                check.id.as_str()
            ));
        }
    }
    Ok(())
}

/// Every exception is valid, names a check of the suite and is accepted once per instance.
pub fn check_exceptions(
    checks: &[EvalDeterministicCheck],
    exceptions: &[EvalAcceptedException],
) -> Result<(), String> {
    if exceptions.len() > MAX_ENTRIES {
        return Err(format!(
            "{EXCEPTIONS_FILE}: at most {MAX_ENTRIES} exceptions"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for e in exceptions {
        e.check().map_err(|m| format!("{EXCEPTIONS_FILE}: {m}"))?;
        if !checks.iter().any(|c| c.id == e.check_id) {
            return Err(format!(
                "{EXCEPTIONS_FILE}: check {} is not a check of the suite",
                e.check_id.as_str()
            ));
        }
        let key = serde_json::to_string(&(&e.check_id, &e.matched_instance)).expect("key");
        if !seen.insert(key) {
            return Err(format!(
                "{EXCEPTIONS_FILE}: check {} has two exceptions for one instance",
                e.check_id.as_str()
            ));
        }
    }
    Ok(())
}

/// One Markdown heading: its level and title.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    let rest = &line[level..];
    (level > 0 && rest.starts_with(' ')).then(|| (level, rest.trim().trim_end_matches('#').trim()))
}

/// The documentation every suite ships with its version (EVAL-13): the [`SECTIONS`] as
/// second-level headings and, under `## Graders`, a `### <check id>` subsection for every check
/// with a `Checks:` line saying what it checks and a `Cannot check:` line saying what it cannot.
pub fn validate_documentation(text: &str, checks: &[EvalDeterministicCheck]) -> Result<(), String> {
    let lines: Vec<&str> = text.lines().collect();
    let at = |title: &str| {
        lines.iter().position(|l| {
            heading(l).is_some_and(|(level, t)| level == 2 && t.eq_ignore_ascii_case(title))
        })
    };
    for section in SECTIONS {
        if at(section).is_none() {
            return Err(format!("{SUITE_FILE} has no \"## {section}\" section"));
        }
    }
    let start = at("Graders").expect("checked above") + 1;
    let end = lines[start..]
        .iter()
        .position(|l| heading(l).is_some_and(|(level, _)| level <= 2))
        .map_or(lines.len(), |n| start + n);
    let graders = &lines[start..end];
    for check in checks {
        let id = check.id.as_str();
        let Some(from) = graders
            .iter()
            .position(|l| heading(l).is_some_and(|(level, t)| level == 3 && t == id))
        else {
            return Err(format!(
                "{SUITE_FILE}: \"## Graders\" has no \"### {id}\" subsection"
            ));
        };
        let body: Vec<&str> = graders[from + 1..]
            .iter()
            .take_while(|l| heading(l).is_none_or(|(level, _)| level > 3))
            .copied()
            .collect();
        let says = |prefix: &str| {
            body.iter().any(|l| {
                l.trim()
                    .strip_prefix(prefix)
                    .is_some_and(|rest| !rest.trim().is_empty())
            })
        };
        if !says("Checks:") {
            return Err(format!(
                "{SUITE_FILE}: grader {id} does not say what it checks (a \"Checks:\" line)"
            ));
        }
        if !says("Cannot check:") {
            return Err(format!(
                "{SUITE_FILE}: grader {id} does not say what it cannot check (a \"Cannot check:\" line)"
            ));
        }
    }
    Ok(())
}

/// The validated contents recorded on a suite revision.
pub fn parsed_of(revision: &StoredInstructionRevision) -> Result<Parsed, StorageError> {
    serde_json::from_value(revision.origin["suite"].clone())
        .map_err(|e| StorageError::InvalidRecord(format!("suite revision: {e}")))
}

/// The resource entry of `path` in a revision, if it has one.
pub fn resource<'a>(revision: &'a StoredInstructionRevision, path: &str) -> Option<&'a Value> {
    revision
        .resources
        .as_array()?
        .iter()
        .find(|r| r["path"] == path)
}
/// The digest of a revision's exceptions file (`None`: it has none).
pub fn exceptions_digest(revision: &StoredInstructionRevision) -> Option<String> {
    resource(revision, EXCEPTIONS_FILE).and_then(|r| r["sha256"].as_str().map(str::to_owned))
}

/// The wire record of one suite revision.
pub fn record(
    item: &StoredInstructionItem,
    revision: &StoredInstructionRevision,
    latest: i64,
    approval_outcome: Option<crate::protocol::records::ApprovalState>,
) -> Result<EvalSuiteRecord, Failure> {
    let parsed = parsed_of(revision)?;
    let documentation: ArtifactRef = serde_json::from_value(revision.content.clone())
        .map_err(|e| StorageError::InvalidRecord(format!("suite documentation: {e}")))?;
    let mut resources = Vec::new();
    for r in revision.resources.as_array().into_iter().flatten() {
        let path = r["path"].as_str().unwrap_or_default();
        // The definition and exceptions files are shown as the record's checks and exceptions.
        if path == DEFINITION_FILE || path == EXCEPTIONS_FILE {
            continue;
        }
        resources.push(EvalSuiteResource {
            path: RelativePath(path.to_owned()),
            part: part(path),
            artifact: serde_json::from_value(r["artifact"].clone())
                .map_err(|e| StorageError::InvalidRecord(format!("suite resource: {e}")))?,
        });
    }
    resources.truncate(MAX_ENTRIES);
    let origin = match item.origin {
        InstructionOrigin::Shared => EvalSuiteOrigin::Shared,
        _ => EvalSuiteOrigin::Harness,
    };
    let location = item.source_path.as_ref().map(|_| RegisteredPath {
        root_id: Id::new(if item.origin == InstructionOrigin::Shared {
            SHARED_ROOT_ID
        } else {
            HARNESS_ROOT_ID
        })
        .expect("static root id"),
        relative: RelativePath(item.name.clone()),
    });
    Ok(EvalSuiteRecord {
        id: item.id.clone(),
        name: label(&item.name, "suite"),
        origin,
        location,
        revision: rev(revision.revision),
        latest_revision: rev(latest.max(revision.revision)),
        active_revision: item.active_revision.map(rev),
        state: match revision.state {
            InstructionRevisionState::Draft => InstructionState::Draft,
            InstructionRevisionState::Proposed => InstructionState::Proposed,
            InstructionRevisionState::Approved => InstructionState::Approved,
            InstructionRevisionState::Rejected => InstructionState::Rejected,
        },
        approved: revision.state == InstructionRevisionState::Approved,
        content_digest: Digest(revision.content_digest.clone()),
        documentation,
        description: parsed
            .description
            .and_then(|d| serde_json::from_value(Value::String(d)).ok()),
        measures: BoundedList(parsed.measures),
        checks: BoundedList(parsed.checks),
        resources: BoundedList(resources),
        exceptions: BoundedList(parsed.exceptions),
        author: Some(revision.author.clone()),
        rationale: revision.origin["rationale"]
            .as_str()
            .and_then(|r| serde_json::from_value(Value::String(r.to_owned())).ok()),
        approval_outcome,
    })
}

/// The validation summary recorded with a revision's provenance.
pub fn validation() -> Value {
    json!({
        "performed": "suite",
        "outcome": "SUITE.md sections and grader notes present; suite.json and exceptions.json valid; no skill carried",
    })
}

/// The digest of a revision whose instruction file and resources are known by their digests.
pub fn revision_digest(documentation: &[u8], resources: &[(String, String, bool)]) -> String {
    let mut sorted = resources.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    scan::revision_digest(&sha256_hex(documentation), &sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(id: &str) -> EvalDeterministicCheck {
        serde_json::from_value(json!({
            "id": id, "ruleRef": "SKILL-01", "measurement": "Proxy",
            "predicate": {"kind": "ToolSucceeded", "operation": "skills.read"},
        }))
        .unwrap()
    }
    fn doc(graders: &str) -> String {
        format!(
            "---\ndescription: Measures reads\n---\n# Reads\n## What it measures\nReads.\n## Graders\n{graders}\n## How to run\nGrade a session.\n## Reading results\nCounts.\n"
        )
    }

    #[test]
    fn documentation_needs_every_section_and_a_note_per_grader() {
        let checks = [check("reads")];
        let good =
            doc("### reads\nChecks: the read succeeded.\nCannot check: whether it was needed.");
        assert!(validate_documentation(&good, &checks).is_ok());
        let missing = good.replace("## How to run", "## Running");
        assert!(
            validate_documentation(&missing, &checks)
                .unwrap_err()
                .contains("How to run")
        );
        let no_limit = doc("### reads\nChecks: the read succeeded.");
        assert!(
            validate_documentation(&no_limit, &checks)
                .unwrap_err()
                .contains("cannot check")
        );
        let other = doc("### writes\nChecks: x\nCannot check: y");
        assert!(
            validate_documentation(&other, &checks)
                .unwrap_err()
                .contains("### reads")
        );
    }

    #[test]
    fn a_suite_carrying_a_skill_or_an_exception_for_an_unknown_check_is_refused() {
        let text = doc("### reads\nChecks: a\nCannot check: b");
        let definition = definition_bytes(&Definition {
            measures: vec![
                serde_json::from_value(json!({"kind": "Skill", "name": "reader"})).unwrap(),
            ],
            checks: vec![check("reads")],
        });
        let parsed = parse(&text, &[(DEFINITION_FILE, &definition)]).unwrap();
        assert_eq!(parsed.description.as_deref(), Some("Measures reads"));
        assert!(
            parse(
                &text,
                &[(DEFINITION_FILE, &definition), ("reader/SKILL.md", b"x")]
            )
            .unwrap_err()
            .contains("carries no skill")
        );
        let exceptions = br#"[{"checkId":"other","matchedInstance":{"sessionId":"s","sequence":"1","entryId":"e","kind":"tool"},"reason":"r","approver":"a","acceptedAtMs":"1"}]"#;
        assert!(
            parse(
                &text,
                &[
                    (DEFINITION_FILE, &definition),
                    (EXCEPTIONS_FILE, exceptions)
                ]
            )
            .unwrap_err()
            .contains("not a check")
        );
    }

    #[test]
    fn parts_follow_the_top_folder() {
        assert!(matches!(part("cases/a.json"), EvalSuitePart::Case));
        assert!(matches!(part("rubrics/r.md"), EvalSuitePart::Rubric));
        assert!(matches!(part("notes.md"), EvalSuitePart::Document));
        assert!(is_suite_file("evals/SUITE.md") && is_suite_file("evals/suite.json"));
        assert!(!is_suite_file("evals/cases.json"));
    }
}
