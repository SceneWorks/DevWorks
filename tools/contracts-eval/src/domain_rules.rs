//! Cross-field architecture invariants only. All live policy/data checks remain host obligations.
use serde_json::Value;
fn present(v: &Value, k: &str) -> bool {
    v.get(k).is_some_and(|x| !x.is_null())
}
fn unique(v: &Value, k: &str) -> bool {
    let Some(a) = v[k].as_array() else {
        return true;
    };
    let mut seen = std::collections::HashSet::new();
    a.iter().all(|x| seen.insert(x.to_string()))
}
fn require(ok: bool, msg: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(msg.into()) }
}
pub fn record(name: &str, v: &Value) -> Result<(), String> {
    match name {
        "ClaimDraft" => require(
            present(v, "objectId") || present(v, "value") || present(v, "valueJson"),
            "claim needs object or value",
        )?,
        "KnowledgeBatch" => require(
            ["entities", "claims", "evidence"]
                .iter()
                .any(|k| v[k].as_array().is_some_and(|a| !a.is_empty())),
            "empty knowledge batch",
        )?,
        "DependencyEdge" => require(v["predecessor"] != v["successor"], "task cannot block itself")?,
        "DependencyReplacement" => require(unique(v, "edges"), "duplicate blocking link")?,
        "PendingFeaturePlan" => {
            if present(v, "approval") {
                require(v["approval"]["state"] != "Approved", "approved draft must be promoted, not remain pending")?;
                require(v["approval"]["planRevision"] == v["revision"], "pending approval must match the current draft")?;
            }
        }
        "WorkRecord" => {
            require(v["kind"] != "Feature" || !present(v, "parentId"), "feature cannot have feature membership")?;
            require(!present(v, "pendingPlan") || v["kind"] == "Feature", "pending feature plan belongs to a feature")?;
            if v["kind"] == "Feature" && present(v, "approval") {
                require(present(v, "plan") && v["approval"]["state"] == "Approved", "active feature approval needs its approved plan")?;
            }
        }
        "ReviewSourceEvidence" if v["state"] == "Completed" => {
            require(present(v, "completedAt") && v["findingsCoverage"]["state"] == "Complete"
                && v["findingsCoverage"]["moreGaps"] == false
                && v["findingsCoverage"]["gaps"].as_array().is_some_and(|a| a.is_empty()),
                "review completion needs terminal time and complete findings coverage")?;
            require(v["findingCount"] != "0" || v["zeroFindingsConfirmed"] == true,
                "zero findings need explicit completion evidence")?;
            require(v["findingCount"] == "0" || v["zeroFindingsConfirmed"] == false,
                "nonzero findings cannot claim zero-findings completion")?;
        }
        "ReviewRecord" if v["state"] == "Ready" => {
            require(v["sources"].as_array().is_some_and(|sources| sources.iter().any(|s|
                s["state"] == "Completed" && s["headDigest"] == v["headDigest"])),
                "ready review needs current-head completed source evidence")?;
        }
        "RequirementItem" => require(
            v["status"] != "Dropped" || present(v, "dropDecision"),
            "dropped requirement needs decision",
        )?,
        "EnvironmentEntry" => require(
            present(v, "value") != present(v, "credentialRef"),
            "environment needs value or credential reference",
        )?,
        "ReceiptObservation" => {
            let recorded = v["state"] == "Recorded";
            require(
                !recorded
                    || [
                        "commandId",
                        "committedSequence",
                        "replyArtifact",
                        "replySchema",
                    ]
                    .iter()
                    .all(|k| present(v, k)),
                "recorded receipt needs durable typed reply",
            )?;
            require(
                recorded || !present(v, "replyArtifact"),
                "unrecorded receipt cannot claim reply",
            )?
        }
        "UsagePage" => {
            require(
                v["hasMore"] == present(v, "nextCursor"),
                "page continuation mismatch",
            )?;
            require(
                !present(v, "itemSequences")
                    || v["itemSequences"].as_array().map(Vec::len)
                        == v["items"].as_array().map(Vec::len),
                "every usage item has its committed sequence",
            )?;
            require(
                !present(v, "nextCursor")
                    || !present(v, "checkpoint")
                    || v["nextCursor"] == v["checkpoint"],
                "a page with more facts continues at its checkpoint",
            )?;
        }
        "UsageItem" => {
            if v["priceKind"] == "Untracked" {
                require(
                    !present(v, "amountUsd"),
                    "untracked usage cannot claim a monetary amount",
                )?
            } else {
                require(present(v, "amountUsd"), "tracked usage needs amount")?
            }
            if v["priceKind"] == "Free" {
                require(
                    v["amountUsd"] == "0" && v["priceOrigin"] == "LocalFree",
                    "free usage requires local-free provenance",
                )?
            }
        }
        "RouteModel" => require(
            v["endpointProfile"] != "SystemOne",
            "decision models cannot enter chat catalog",
        )?,
        "RouteChoice" => require(
            v["requestedEffort"] == "Default" || v["requestedEffort"] == v["effectiveEffort"],
            "explicit effort cannot silently change",
        )?,
        "DecisionQuestion" => {}
        _ => {}
    }
    Ok(())
}
pub fn input(op: &str, mode: &str, v: &Value) -> Result<(), String> {
    for k in [
        "repositoryIds",
        "orderedWorkIds",
        "orderedTaskIds",
        "orderedStatusIds",
        "memberIds",
        "toolIds",
        "messageIds",
    ] {
        require(unique(v, k), "duplicate identity in set")?
    }
    if op == "history.tool_usage" {
        // Exactly one scope: a root, or (revision 2) a project or the workspace spelled `true`.
        let named = ["rootId", "projectId", "workspace"]
            .iter()
            .filter(|k| present(v, k))
            .count();
        require(
            named == 1 && (!present(v, "workspace") || v["workspace"] == true),
            "usage names exactly one of a root, a project and the workspace",
        )?
    }
    if op.ends_with(".configure") || op.ends_with(".draft") {
        let key = if present(v, "serverId") {
            Some("serverId")
        } else if present(v, "connectionId") {
            Some("connectionId")
        } else if present(v, "instructionId") {
            Some("instructionId")
        } else {
            None
        };
        if key.is_some() {
            require(
                present(v, "expectedRevision"),
                "edit needs expected revision",
            )?
        }
    }
    match op {
        "tasks.set_feature" => require(
            present(v, "featureId") != (v["clearFeature"] == true),
            "feature set/clear must be exclusive",
        )?,
        "tasks.dependency_links" => {
            if let Some(edges) = v["replacement"]["edges"].as_array() {
                require(edges.iter().all(|edge| edge["successor"] == v["taskId"]), "replacement may only change target task prerequisites")?;
            }
        }
        "tasks.assign" => require(
            present(v, "assignmentId") != (v["clearAssignment"] == true),
            "assignment set/clear must be exclusive",
        )?,
        "tasks.sync_plan" => {
            require(unique(v, "tasks"), "duplicate task entries")?;
            require(
                !(present(v, "projectPlan") && present(v, "stagedProjectPlan")),
                "inline and staged plan are exclusive",
            )?
        }
        "workflows.reassign_and_remove" => require(
            v["removeStatusId"] != v["destinationStatusId"],
            "status destination must differ",
        )?,
        "documents.import" => require(
            present(v, "replaceDocumentId") == present(v, "expectedRevision"),
            "replacement needs exact target revision",
        )?,
        "knowledge.entity_bundle" | "knowledge.entity_graph" => require(
            v["query"]["entityIds"]
                .as_array()
                .is_some_and(|a| a.len() == 1),
            "entity operation needs one entity ID",
        )?,
        "knowledge.claim_bundle" => require(
            v["query"]["claimIds"]
                .as_array()
                .is_some_and(|a| a.len() == 1),
            "claim bundle needs one claim ID",
        )?,
        "knowledge.expand_frontier" => require(
            ["entityIds", "claimIds"]
                .iter()
                .any(|k| v["query"][k].as_array().is_some_and(|a| !a.is_empty())),
            "frontier must not become unrestricted scan",
        )?,
        "knowledge.tag" => {
            let ids = v["claimIds"].as_array().unwrap();
            let rs = v["expectedRevisions"].as_array().unwrap();
            require(
                ids.len() == rs.len() && ids.iter().all(|id| rs.iter().any(|r| r["id"] == *id)),
                "tag revisions must match selected claims",
            )?
        }
        "router.configure" => {
            let a = &v["action"];
            if a["kind"] == "Model" {
                require(
                    a["model"]["endpointProfile"] != "SystemOne",
                    "SystemOne cannot be a conversational model",
                )?
            }
            if a["kind"] == "Tier" {
                require(unique(a, "memberIds"), "duplicate tier member")?
            }
        }
        "router.decide" => {
            let a = v["questions"].as_array().unwrap();
            let mut seen = std::collections::HashSet::new();
            for q in a {
                require(
                    seen.insert(q["name"].to_string()),
                    "duplicate decision question",
                )?;
                let question = &q["question"];
                if question["kind"] == "Choice" {
                    let mut names = std::collections::HashSet::new();
                    for criterion in question["criteria"].as_array().unwrap() {
                        require(
                            names.insert(criterion["name"].to_string()),
                            "duplicate criterion name",
                        )?
                    }
                }
                match question["kind"].as_str() {
                    Some("Choice") | Some("Score") => require(
                        question["criteria"]
                            .as_array()
                            .is_some_and(|c| c.len() >= 2)
                            && unique(question, "criteria"),
                        "decision criteria need at least two unique choices",
                    )?,
                    _ => {}
                }
            }
        }
        "github.account" if mode == "Command" => {
            require(v["action"] != "Inspect", "account inspection is a Query")?
        }
        "queue.retry" => require(
            v["resolution"] == "Retry",
            "retry operation/action mismatch",
        )?,
        "queue.reconcile" => require(
            v["resolution"] == "Reconcile",
            "reconcile operation/action mismatch",
        )?,
        _ => {}
    }
    if op == "settings.save_revision" || op == "settings.validate" {
        if let Some(entries) = v["entries"].as_array() {
            for entry in entries {
                let key = entry["key"].as_str().unwrap_or("");
                if key == "github.issueTriageIntervalSeconds" || key == "reviews.automationWaitSeconds" {
                    require(entry["value"]["value"].as_u64().is_some_and(|n| n > 0 && n <= i64::MAX as u64),
                        "interval must be a positive signed-64 integer")?;
                    if key == "github.issueTriageIntervalSeconds" {
                        require(v["scope"]["kind"] == "Workspace", "triage interval is app-wide only")?;
                    }
                }
            }
        }
    }
    if op.starts_with("maintenance.") && v["previewOnly"] == false {
        require(
            present(v, "previewId") && present(v, "expectedRevision"),
            "prune needs exact preview and revision",
        )?
    }
    Ok(())
}

pub fn result(op: &str, v: &Value) -> Result<(), String> {
    let value = &v["value"];
    if op.starts_with("code.") && op != "code.set_trust" || op == "repositories.nodes" {
        let kind = value["result"]["kind"].as_str().unwrap_or("");
        let allowed = match op {
            "code.source" | "code.snippet" => kind == "Source",
            "code.graph_schema" => kind == "Schema",
            "code.graph_scope_status" => kind == "ScopeStatus",
            _ => kind == "Nodes" || kind == "Graph",
        };
        require(
            allowed,
            "graph result discriminator does not match operation",
        )?;
    }
    let family = op.split('.').next().unwrap_or("");
    let expected = match family {
        "features" => Some("Feature"),
        "issues" => Some("Issue"),
        "tasks" if op != "tasks.sync_plan" => Some("TaskOrIssue"),
        _ => None,
    };
    if let Some(expected) = expected {
        if let Some(kind) = value.get("kind") {
            require(kind == expected || expected == "TaskOrIssue" && (kind == "Task" || kind == "Issue"), "work kind does not match operation")?
        }
        if let Some(items) = value["items"].as_array() {
            for x in items {
                if let Some(kind) = x.get("kind") {
                    require(kind == expected || expected == "TaskOrIssue" && (kind == "Task" || kind == "Issue"), "work list kind does not match operation")?
                }
            }
        }
    }
    Ok(())
}
