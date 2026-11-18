//! Sample frames of the work builders and of each request kind the Features, board, Issues,
//! Overview and workflow screens send.
use super::{Silent, from};
use crate::shell::work::workflow::{
    AllocationFilters, create_status_request, order_status_request, release_request,
    remove_status_request, rename_status_request,
};
use crate::shell::work::{
    CreateForm, EditForm, MessageDraft, PlanForm, approval_request, create_request, edit_request,
    message_post_body, move_request, open_plan_form, order_request, plan_request, unlink_request,
};
use crate::usage_screen::BudgetDraft;
use crate::work_ops as wops;
use crate::work_screen::adjacent_move;
use crate::work_screen::detail::{
    BlockerEdit, assign_request, blocker_change, feature_status_request, set_feature_request,
};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    AllocationClaimRecord, AllocationQueueSnapshot, RootSnapshot, StatusRecord, WorkRecord,
};
use serde_json::json;

fn artifact() -> ArtifactRef {
    from(json!({"artifactId":"artifact-a","versionId":"version-a",
        "sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/plain"}))
}

fn record(kind: &str, id: &str, extra: serde_json::Value) -> WorkRecord {
    let mut v = json!({
        "id": id, "revision": "3", "projectId": "project-a", "kind": kind,
        "title": "Checkout", "description": "About", "statusId": "status-a", "priority": 5,
        "repositoryIds": [], "dependencyIds": [], "goals": [], "requirements": [],
        "acceptance": [], "inheritsRequirementIds": [], "touches": [], "links": [],
        "comments": [], "handoffs": [], "attempts": [], "mergedAwaitingAcceptance": false,
        "detailCoverage": {"state": "Complete", "gaps": [], "moreGaps": false},
        "commentCount": "0", "handoffCount": "0", "attemptCount": "0", "dependentIds": [],
        "workflowRevision": "4"});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    from(v)
}

fn feature() -> WorkRecord {
    let a = artifact();
    let draft = json!({"content": a,
        "acceptance": [{"id": "A1", "text": "works\non every browser",
            "evidenceIds": ["ev-1"], "state": "Met"}],
        "repositoryIds": ["repo-a"], "goals": ["ship\nthis quarter"],
        "requirements": [
            {"id": "E1", "statement": "pay", "status": "Proposed", "coveredByTaskIds": ["task-1"]},
            {"id": "E2", "statement": "refunds", "status": "Dropped", "dropDecision": a,
             "coveredByTaskIds": []},
            {"id": "E4", "statement": "tax", "status": "Confirmed", "coveredByTaskIds": []}],
        "acceptanceTests": [{"id": "T1", "text": "checkout e2e", "evidenceIds": [],
            "state": "Unmet"}]});
    record(
        "Feature",
        "feature-a",
        json!({"queueRevision": "9", "labels": ["one"],
            "pendingPlan": {"revision": "2", "draft": draft,
                "planDigest": "d".repeat(64), "budget": {"kind": "Limit", "microUsd": "2500000"}}}),
    )
}

fn task() -> WorkRecord {
    record(
        "Task",
        "task-a",
        json!({"parentId": "feature-a", "dependencyIds": ["a", "b"], "queueRevision": "7",
            "labels": ["x"], "taskType": "Bug"}),
    )
}

fn snapshot(holds: bool) -> RootSnapshot {
    let blockers = if holds {
        json!([
            {"holdId": "hold-b", "revision": "2", "subject": {"kind": "root", "id": "feature-a"},
                "cause": "HumanPause"},
            {"holdId": "hold-c", "revision": "3", "subject": {"kind": "root", "id": "feature-a"},
                "cause": "Approval"},
            {"holdId": "hold-a", "revision": "4", "subject": {"kind": "root", "id": "feature-a"},
                "cause": "BudgetContinue"}])
    } else {
        json!([])
    };
    from(
        json!({"root": {"rootId": "feature-a", "revision": "6", "desiredState": "Paused"},
        "blockers": {"items": blockers, "hasMore": false}}),
    )
}

fn status(id: &str) -> StatusRecord {
    from(
        json!({"id": id, "revision": "2", "label": "Review", "category": "Active",
        "rank": "1", "workflowRevision": "8", "affectedWorkDigest": "e".repeat(64),
        "affectedWorkCount": "3"}),
    )
}

fn claim() -> AllocationClaimRecord {
    from(
        json!({"id": "claim-a", "workId": "task-a", "ownerId": "actor-a", "fence": "5",
        "expiresAtMs": "10", "state": "Held"}),
    )
}

fn queue() -> AllocationQueueSnapshot {
    from(json!({"projectId": "project-a", "queueRevision": "11", "workflowRevision": "12"}))
}

fn form(kind: &str) -> CreateForm {
    CreateForm {
        kind: kind.into(),
        title: "A title".into(),
        description: "A description".into(),
        feature: Some("feature-a".into()),
        priority: 3,
        task_type: Some("Story".into()),
        labels: vec!["l1".into(), "l2".into()],
        budget: BudgetDraft {
            enabled: true,
            dollars: "12.5".into(),
        },
    }
}

fn edit_form(base: WorkRecord) -> EditForm {
    EditForm {
        snapshot: base,
        title: "Renamed".into(),
        description: "Changed".into(),
        priority: 9,
        task_type: Some("Story".into()),
        labels: vec!["z".into()],
        budget: BudgetDraft {
            enabled: true,
            dollars: "1".into(),
        },
        budget_changed: true,
    }
}

fn plan_form() -> PlanForm {
    let mut form = open_plan_form(&feature());
    form.requirements[0].text = "pay by card\nor wallet".into();
    form.requirements.push(crate::shell::work::Entry {
        id: " E5 ".into(),
        text: "new".into(),
    });
    form.statuses.insert("E1".into(), from(json!("Confirmed")));
    form.goals.push("  ".into());
    form
}

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let f = feature();
    let t = task();
    let pending = f.pending_plan.clone().unwrap();
    let digest = pending.plan_digest.clone().unwrap();
    let draft = MessageDraft {
        body: "Hello".into(),
        ..Default::default()
    };
    let full = MessageDraft {
        body: "Why?".into(),
        kind: 1,
        to_session: " session-a ".into(),
        to_human: true,
        item: "task-a".into(),
        feature: "feature-a".into(),
        resource: "gpu".into(),
        blocking: true,
        reply_to: "message-a".into(),
        resolution: 2,
    };
    let mut no_budget = plan_form();
    no_budget.budget = None;
    let mut budget = plan_form();
    budget.budget = Some(BudgetDraft {
        enabled: true,
        dollars: "3".into(),
    });
    let mut dropped = plan_form();
    dropped.drops.insert(
        "E1".into(),
        crate::shell::work::DropDraft {
            reason: "no".into(),
            artifact: Some(artifact()),
        },
    );
    let mut restored = open_plan_form(&f);
    restored.base.requirements[1].status = from(json!("Dropped"));
    restored.requirements.push(crate::shell::work::Entry {
        id: "E2".into(),
        text: "refunds again".into(),
    });
    let mut no_label = edit_form(t.clone());
    no_label.budget_changed = false;
    let mut cleared = edit_form(t.clone());
    cleared.task_type = None;
    let mut off = form("feature");
    off.budget.enabled = false;
    let content = artifact();
    let bucket = ["w1", "w2", "w3"];
    let move_up = adjacent_move(&bucket, 1, true).unwrap();
    let move_down = adjacent_move(&bucket, 1, false).unwrap();
    let ids = [from(json!("s1")), from(json!("s2"))];
    vec![
        (
            "work_message_post",
            wops::message_post(b, message_post_body(&draft, "project-a")),
        ),
        (
            "work_message_post_full",
            wops::message_post(b, message_post_body(&full, "project-a")),
        ),
        (
            "work_list_features",
            wops::work_list(b, wops::Family::Features, "project-a", None),
        ),
        (
            "work_list_issues_cursor",
            wops::work_list(b, wops::Family::Issues, "project-a", Some("c1")),
        ),
        (
            "work_list_tasks",
            wops::work_list(b, wops::Family::Tasks, "project-a", None),
        ),
        (
            "work_get_features",
            wops::work_get(b, wops::Family::Features, "feature-a"),
        ),
        (
            "work_get_issues",
            wops::work_get(b, wops::Family::Issues, "issue-a"),
        ),
        (
            "work_get_tasks",
            wops::work_get(b, wops::Family::Tasks, "task-a"),
        ),
        (
            "work_create_feature",
            wops::work_command(
                b,
                create_request(&form("feature"), "project-a", "backlog-a")
                    .unwrap()
                    .1,
            ),
        ),
        (
            "work_create_feature_off",
            wops::work_command(b, create_request(&off, "project-a", "backlog-a").unwrap().1),
        ),
        (
            "work_create_issue",
            wops::work_command(
                b,
                create_request(&form("issue"), "project-a", "backlog-a")
                    .unwrap()
                    .1,
            ),
        ),
        (
            "work_create_task",
            wops::work_command(
                b,
                create_request(&form("task"), "project-a", "backlog-a")
                    .unwrap()
                    .1,
            ),
        ),
        (
            "work_edit_feature",
            wops::work_command(b, edit_request(&edit_form(f.clone())).unwrap().unwrap().1),
        ),
        (
            "work_edit_task",
            wops::work_command(b, edit_request(&no_label).unwrap().unwrap().1),
        ),
        (
            "work_edit_task_clear_type",
            wops::work_command(b, edit_request(&cleared).unwrap().unwrap().1),
        ),
        (
            "work_move",
            wops::work_command(b, move_request(&t, "status-b")),
        ),
        (
            "work_order_features",
            wops::work_command(
                b,
                order_request(
                    "features.prioritize",
                    "project-a",
                    None,
                    None,
                    move_up.clone(),
                    "7",
                ),
            ),
        ),
        (
            "work_order_issues",
            wops::work_command(
                b,
                order_request(
                    "issues.prioritize",
                    "project-a",
                    None,
                    None,
                    move_down.clone(),
                    "7",
                ),
            ),
        ),
        (
            "work_order_tasks",
            wops::work_command(
                b,
                order_request(
                    "tasks.prioritize",
                    "project-a",
                    Some("feature-a"),
                    Some(5),
                    move_up,
                    "7",
                ),
            ),
        ),
        (
            "work_order_tasks_standalone",
            wops::work_command(
                b,
                order_request("tasks.prioritize", "project-a", None, None, move_down, "7"),
            ),
        ),
        (
            "work_request_approval",
            wops::work_command(b, approval_request(&f, &pending, &digest)),
        ),
        (
            "work_edit_plan_budget",
            wops::work_command(b, plan_request(&budget, &content).unwrap()),
        ),
        (
            "work_edit_plan_kept_proposal",
            wops::work_command(b, plan_request(&plan_form(), &content).unwrap()),
        ),
        (
            "work_edit_plan_clear_budget",
            wops::work_command(b, plan_request(&no_budget, &content).unwrap()),
        ),
        (
            "work_edit_plan_drop",
            wops::work_command(b, plan_request(&dropped, &content).unwrap()),
        ),
        (
            "work_edit_plan_restore",
            wops::work_command(b, plan_request(&restored, &content).unwrap()),
        ),
        ("work_unlink", wops::work_command(b, unlink_request(&t))),
        (
            "work_feature_status",
            wops::work_command(b, feature_status_request(&f, "status-b")),
        ),
        (
            "work_set_feature",
            wops::work_command(b, set_feature_request(&t, "feature-b")),
        ),
        (
            "work_assign_clear",
            wops::work_command(b, assign_request(&t, None)),
        ),
        (
            "work_assign",
            wops::work_command(b, assign_request(&t, Some(&from(json!("assignment-a"))))),
        ),
        (
            "work_blocker_add",
            wops::work_command(b, blocker_change(&t, BlockerEdit::Add("c")).unwrap()),
        ),
        (
            "work_blocker_remove",
            wops::work_command(b, blocker_change(&t, BlockerEdit::Remove("a")).unwrap()),
        ),
        (
            "work_history_project",
            wops::history_events(b, "project-a", None, None),
        ),
        (
            "work_history_root_after",
            wops::history_events(b, "project-a", Some("feature-a"), Some("cursor-1")),
        ),
        ("work_run_inspect", wops::run_inspect(b, "feature-a")),
        (
            "work_run_start",
            wops::run_control(b, "runs.start", &f, &snapshot(false)),
        ),
        (
            "work_run_pause",
            wops::run_control(b, "runs.pause", &f, &snapshot(false)),
        ),
        (
            "work_run_continue",
            wops::run_control(b, "runs.continue", &f, &snapshot(true)),
        ),
        ("work_git_links", wops::git_links(b, "feature-a", None)),
        (
            "work_git_links_cursor",
            wops::git_links(b, "feature-a", Some("cursor-1")),
        ),
        (
            "work_workflow_list",
            wops::workflow_list(b, "project-a", None),
        ),
        (
            "work_workflow_list_cursor",
            wops::workflow_list(b, "project-a", Some("cursor-1")),
        ),
        (
            "work_workflow_order",
            wops::workflow_command(
                b,
                order_status_request("project-a", &Some("8".into()), &ids),
            ),
        ),
        (
            "work_workflow_create",
            wops::workflow_command(
                b,
                create_status_request("project-a", "8", "Review", "Active", None),
            ),
        ),
        (
            "work_workflow_create_after",
            wops::workflow_command(
                b,
                create_status_request("project-a", "8", "Review", "Active", Some("status-a")),
            ),
        ),
        (
            "work_workflow_rename",
            wops::workflow_command(
                b,
                rename_status_request("project-a", &status("status-a"), "Renamed"),
            ),
        ),
        (
            "work_workflow_remove",
            wops::workflow_command(
                b,
                remove_status_request("project-a", &status("status-a"), &Some("status-b".into())),
            ),
        ),
        (
            "work_allocation_claim_project",
            wops::allocation_command(
                b,
                AllocationFilters::default()
                    .claim("project-a", None, &queue(), 60000)
                    .1,
            ),
        ),
        (
            "work_allocation_claim_feature",
            wops::allocation_command(
                b,
                AllocationFilters::default()
                    .claim("project-a", Some("feature-a"), &queue(), 60000)
                    .1,
            ),
        ),
        (
            "work_allocation_claim_filters",
            wops::allocation_command(
                b,
                AllocationFilters {
                    repository_id: Some("repo-a".into()),
                    task_type: Some("Bug".into()),
                    label: "urgent".into(),
                    another: true,
                    idempotency_key: "key-1".into(),
                }
                .claim("project-a", None, &queue(), 60000)
                .1,
            ),
        ),
        (
            "work_allocation_claim_feature_filters",
            wops::allocation_command(
                b,
                AllocationFilters {
                    label: "urgent".into(),
                    ..Default::default()
                }
                .claim("project-a", Some("feature-a"), &queue(), 1)
                .1,
            ),
        ),
        (
            "work_allocation_release",
            wops::allocation_command(b, release_request(&claim())),
        ),
    ]
}
