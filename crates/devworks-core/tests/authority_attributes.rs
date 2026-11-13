//! Authority rules are attributes of the operation descriptor, generated from
//! `tools/contracts-eval/domain-spec.json` (`operationAuthority`). The lists below are a frozen
//! copy of the operation-name lists `authority/policy.rs` and `authority/service.rs` held before
//! the rules moved onto the descriptor; they prove every existing operation keeps its behaviour,
//! except the recorded native-execution decisions.
mod common;

use devworks_core::registry::{self, AssignmentScope, Mode, OperationDescriptor};
use serde_json::Value;
use std::collections::BTreeSet;

const OLD_NATIVE_EXECUTION: [&str; 18] = [
    "git.worktree",
    "git.commit",
    "git.push",
    "process.execute",
    "watch.create",
    "process.install_project_dependencies",
    "containers.build",
    "containers.pull",
    "containers.create",
    "containers.start",
    "containers.exec",
    "mcp.connect",
    "mcp.discover",
    "addons.install",
    "addons.update",
    "addons.enable",
    "addons.rollback",
    "addons.configure",
];
const OLD_APPROVAL_REQUIRED: [&str; 5] = [
    "projects.archive",
    "projects.delete",
    "repositories.remove_index",
    "skills.rollback",
    "roles.rollback",
];
const OLD_HUMAN_ONLY: [&str; 7] = [
    "agents.history",
    "agents.events",
    "mcp.configure",
    "mcp.select_tools",
    "mcp.classify_tool",
    "mcp.disable",
    "mcp.remove",
];
/// Refused to an agent with a root (`registered_tool_visibility`).
const OLD_ROOT_DENIED: [&str; 10] = [
    "features.list",
    "features.create",
    "features.prioritize",
    "features.dependencies",
    "issues.list",
    "issues.create",
    "issues.prioritize",
    "tasks.list",
    "tasks.prioritize",
    "tasks.dependency_links",
];
/// Refused to an agent with an assignment (`artifacts.list`, `jobs.*` and two approval views).
fn old_assignment_denied(op: &str) -> bool {
    op == "artifacts.list"
        || op.starts_with("jobs.")
        || matches!(op, "approvals.list" | "approvals.review_queue")
}
/// Commands refused to a root-bound agent (besides the Configuration authority class).
fn old_administration(op: &str) -> bool {
    matches!(family(op), "projects" | "workflows" | "roles")
        || op == "repositories.remove_index"
        || op == "next_work.claim_project_queue"
}
/// Refused to a root-bound Worker.
const OLD_WORKER_DENIED: [&str; 11] = [
    "runs.start",
    "runs.pause",
    "runs.continue",
    "runs.redirect",
    "runs.stop",
    "tasks.create",
    "tasks.sync_plan",
    "features.edit",
    "features.edit_plan",
    "features.request_plan_approval",
    "next_work.claim_feature_task",
];

/// Which typed-input check `agent_assignment_scope` applied to a root-bound agent.
#[derive(Debug, PartialEq, Eq)]
enum InputCheck {
    /// Not checked: every root-bound agent passed.
    None,
    /// Checked; a root lineage without an assignment was refused.
    Root,
    /// Checked only once an assignment was bound.
    Assignment,
}
fn old_input_check(op: &str) -> InputCheck {
    if matches!(
        family(op),
        "tasks" | "features" | "issues" | "next_work" | "runs" | "history" | "indexing"
    ) {
        InputCheck::Root
    } else if matches!(family(op), "artifacts" | "jobs" | "approvals")
        || matches!(
            op,
            "git.commit"
                | "git.pull_request"
                | "browser.navigate"
                | "browser.upload"
                | "sessions.append_message"
                | "knowledge.diagnostics"
                | "knowledge.import_user_data"
        )
    {
        InputCheck::Assignment
    } else {
        InputCheck::None
    }
}
fn new_input_check(scope: AssignmentScope) -> InputCheck {
    match scope {
        AssignmentScope::Project | AssignmentScope::Administration => InputCheck::None,
        AssignmentScope::Root | AssignmentScope::Coordinator | AssignmentScope::RootDenied => {
            InputCheck::Root
        }
        AssignmentScope::Assignment | AssignmentScope::AssignmentDenied => InputCheck::Assignment,
    }
}

fn family(op: &str) -> &str {
    op.split_once('.').map_or(op, |(f, _)| f)
}
fn command_only(d: &OperationDescriptor) -> bool {
    d.modes.iter().all(|b| b.mode == Mode::Command)
}

/// The recorded native-execution decisions that changed behaviour (each now needs a reviewed
/// native grant under Project scope). `git.pull_request` was reviewed and stays non-native.
const NEWLY_NATIVE: [&str; 7] = [
    "git.integrate",
    "git.cleanup",
    "desktop.input",
    "browser.sessions",
    "database.execute_write",
    "database.apply_migration",
    "database.direct_schema_change",
];

#[test]
fn descriptor_attributes_reproduce_the_former_name_lists() {
    for d in registry::all() {
        let op = d.name;
        let native = OLD_NATIVE_EXECUTION.contains(&op) || NEWLY_NATIVE.contains(&op);
        assert_eq!(d.native_execution, native, "{op} native execution");
        assert_eq!(
            d.approval_required,
            OLD_APPROVAL_REQUIRED.contains(&op),
            "{op} approval required"
        );
        assert_eq!(
            d.human_only,
            OLD_HUMAN_ONLY.contains(&op),
            "{op} human only"
        );
        let scope = d.assignment_scope;
        assert_eq!(
            scope == AssignmentScope::RootDenied,
            OLD_ROOT_DENIED.contains(&op),
            "{op} root denied"
        );
        assert_eq!(
            scope == AssignmentScope::AssignmentDenied,
            old_assignment_denied(op),
            "{op} assignment denied"
        );
        assert_eq!(
            scope == AssignmentScope::Coordinator,
            OLD_WORKER_DENIED.contains(&op),
            "{op} coordinator only"
        );
        // The administration rule only ever refused commands.
        if d.binding(Mode::Command).is_some() {
            assert_eq!(
                scope == AssignmentScope::Administration,
                old_administration(op),
                "{op} administration"
            );
        } else {
            assert_ne!(scope, AssignmentScope::Administration, "{op} is a query");
        }
        let old = old_input_check(op);
        let new = new_input_check(scope);
        if old != new {
            // The administration refusal precedes the input check, so a root-bound agent never
            // reached the former check of a command-only administration operation.
            assert!(
                scope == AssignmentScope::Administration && command_only(d),
                "{op} input check {old:?} became {new:?}"
            );
        }
    }
    // The recorded native-execution decisions.
    for op in NEWLY_NATIVE {
        assert!(registry::find(op).unwrap().native_execution, "{op}");
    }
    assert!(!registry::find("git.pull_request").unwrap().native_execution);
}

#[test]
fn every_registered_operation_states_its_authority_attributes_in_the_spec() {
    let spec: Value = serde_json::from_str(
        &std::fs::read_to_string(common::repo_file("tools/contracts-eval/domain-spec.json"))
            .unwrap(),
    )
    .unwrap();
    let recorded = spec["operationAuthority"].as_object().unwrap();
    let registered: BTreeSet<&str> = registry::all().iter().map(|d| d.name).collect();
    assert_eq!(
        recorded.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        registered,
        "every registered operation, and nothing else, has an assignment-scope rule"
    );
    for d in registry::all() {
        let a = &recorded[d.name];
        let scope = match a["assignmentScope"].as_str().unwrap() {
            "project" => AssignmentScope::Project,
            "root" => AssignmentScope::Root,
            "coordinator" => AssignmentScope::Coordinator,
            "assignment" => AssignmentScope::Assignment,
            "rootDenied" => AssignmentScope::RootDenied,
            "assignmentDenied" => AssignmentScope::AssignmentDenied,
            "administration" => AssignmentScope::Administration,
            other => panic!("{}: unknown assignment scope {other}", d.name),
        };
        assert_eq!(d.assignment_scope, scope, "{}", d.name);
        assert_eq!(Some(d.native_execution), a["nativeExecution"].as_bool());
        assert_eq!(Some(d.approval_required), a["approvalRequired"].as_bool());
        assert_eq!(Some(d.human_only), a["humanOnly"].as_bool());
    }
}
