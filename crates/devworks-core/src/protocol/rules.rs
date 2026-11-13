//! Recorded pure cross-field invariants (tools/contracts-eval/src/domain_rules.rs), applied after
//! strict typed decoding. Live relationship, authority and policy checks remain host obligations.
//! Missing arrays are rejected rather than unwrapped so a malformed value can never panic.
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
/// `a <= b` for two decimal-string counters, when both are present.
fn ordered(v: &Value, a: &str, b: &str) -> bool {
    let n = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u128>().ok())
    };
    match (n(a), n(b)) {
        (Some(x), Some(y)) => x <= y,
        _ => true,
    }
}

fn code_record(name: &str, v: &Value) -> Result<(), String> {
    let number = |key: &str| v[key].as_str().and_then(|n| n.parse::<u64>().ok());
    if name == "CodeSearchSelection" && v["mode"] != "Unified" {
        require(
            v["text"].as_str().is_some_and(|s| !s.trim().is_empty()),
            "node search requires a name pattern",
        )?;
    }
    if name == "CodeProperty" {
        let (Some(offset), Some(total), Some(text)) = (
            number("offset"),
            number("totalBytes"),
            v["jsonText"].as_str(),
        ) else {
            return Err("invalid property fragment".into());
        };
        let end = offset
            .checked_add(text.len() as u64)
            .ok_or("property offset overflow")?;
        require(end <= total, "property fragment exceeds its value")?;
        let next = v["nextOffset"].as_str().and_then(|n| n.parse::<u64>().ok());
        require(
            if end < total {
                !text.is_empty() && next == Some(end)
            } else {
                next.is_none()
            },
            "property continuation mismatch",
        )?;
        if offset == 0 && end == total {
            serde_json::from_str::<Value>(text).map_err(|_| "invalid complete property JSON")?;
        }
    }
    if name == "CodeSourceText" {
        let (Some(start), Some(end), Some(total), Some(text)) = (
            number("startByte"),
            number("endByte"),
            number("totalBytes"),
            v["text"].as_str(),
        ) else {
            return Err("invalid source fragment".into());
        };
        require(
            start <= end && end <= total && end - start == text.len() as u64,
            "source byte range mismatch",
        )?;
        require(
            if total == 0 {
                number("firstLine") == Some(0) && number("lastLine") == Some(0)
            } else {
                number("firstLine").is_some_and(|n| n > 0) && ordered(v, "firstLine", "lastLine")
            },
            "invalid source lines",
        )?;
        require(
            present(v, "nodeStartLine") == present(v, "nodeEndLine")
                && ordered(v, "nodeStartLine", "nodeEndLine"),
            "invalid highlighted node range",
        )?;
    }
    if name == "CodeSourceLocation" {
        require(
            ordered(v, "startLine", "endLine")
                && present(v, "startByte") == present(v, "endByte")
                && ordered(v, "startByte", "endByte"),
            "invalid code location",
        )?;
    }
    if name == "CodeImpactSummary" {
        let total = ["criticalCount", "highCount", "mediumCount", "lowCount"]
            .iter()
            .try_fold(0u64, |sum, key| {
                number(key).and_then(|n| sum.checked_add(n))
            });
        require(
            total == number("totalAffected"),
            "impact risk counts disagree",
        )?;
    }
    if name == "CodeNode" {
        require(unique(v, "properties"), "duplicate property fragment")?;
    }
    if matches!(name, "CodeSearchResult" | "CodeNodesResult")
        && let Some(total) = number("total")
    {
        let rows = if name == "CodeSearchResult" {
            "items"
        } else {
            "nodes"
        };
        require(
            v[rows].as_array().is_some_and(|a| a.len() as u64 <= total),
            "graph page exceeds total",
        )?;
    }
    if matches!(name, "CodeNode" | "CodeSourceLocation" | "CodeEdge") {
        for key in ["unitKey", "sourceUnitKey", "targetUnitKey", "producingUnit"] {
            if let Some(unit) = v[key].as_str() {
                require(
                    !unit.contains('\\'),
                    "graph unit keys use canonical separators",
                )?;
            }
        }
    }

    if name == "CodeReadCoverage" {
        let missing = v["unavailableRepositories"]
            .as_array()
            .ok_or("missing repository coverage rows")?;
        let gaps = v["coverage"]["gaps"]
            .as_array()
            .ok_or("missing graph coverage gaps")?;
        let mut ids = std::collections::HashSet::new();
        require(
            missing
                .iter()
                .all(|r| ids.insert(r["repositoryId"].as_str())),
            "duplicate unavailable repository",
        )?;
        if v["coverage"]["state"] == "Complete" {
            require(
                missing.is_empty() && gaps.is_empty() && v["coverage"]["moreGaps"] == false,
                "complete graph scope cannot hide missing repositories",
            )?;
        }
        if v["coverage"]["state"] == "Partial" {
            require(
                !missing.is_empty() || !gaps.is_empty() || v["coverage"]["moreGaps"] == true,
                "partial graph scope needs explicit omissions",
            )?;
        }
    }
    if name == "CodeModuleIdentity" {
        require(
            v["moduleName"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty()),
            "module identity requires its actual name",
        )?;
    }
    if name == "CodeModuleDrilldown" {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "module drilldown continuation mismatch",
        )?;
        if let Some(total) = number("count") {
            require(
                v["nodes"]
                    .as_array()
                    .is_some_and(|rows| rows.len() as u64 <= total),
                "module rows exceed observed count",
            )?;
        }
    }
    if name == "CodeModuleAnalysis" {
        require(
            (v["state"] == "Recorded") == present(v, "recorded"),
            "only an actual recorded module analysis supplies summary or semantic results",
        )?;
        if v["state"] != "Recorded" {
            require(
                present(v, "reason"),
                "unrecorded module analysis needs its actual reason",
            )?;
        }
        if v["observedCoverage"]["state"] == "Unavailable" {
            require(
                v["observedNodeCounts"]
                    .as_array()
                    .is_some_and(|r| r.is_empty()),
                "unavailable observed module counts are unknown",
            )?;
            for key in ["endpoints", "services", "tables"] {
                require(
                    !present(&v[key], "count")
                        && v[key]["nodes"].as_array().is_some_and(|r| r.is_empty())
                        && v[key]["hasMore"] == false,
                    "unavailable module drilldown cannot invent zero totals or node references",
                )?;
            }
        }
    }
    Ok(())
}
fn convention_record(name: &str, v: &Value) -> Result<(), String> {
    let text = |key: &str, max: usize| {
        v[key]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= max && !s.contains('\0'))
    };
    if name == "ConventionRecord" {
        require(
            [("title", 500), ("slug", 200)].iter().all(|(key, max)| {
                v[*key].as_str().is_some_and(|s| {
                    (*key == "slug" || !s.trim().is_empty()) && s.chars().count() <= *max
                })
            }),
            "invalid convention title or slug",
        )?;
        require(
            v["sortOrder"]
                .as_str()
                .and_then(|s| s.parse::<i32>().ok())
                .is_some(),
            "convention order must fit its retained signed integer",
        )?;
        require(
            v["lifecycle"] == "Deleted" || present(v, "content"),
            "live convention needs actual content",
        )?;
    }
    if name == "ConventionSectionPath" {
        require(
            present(v, "text") != present(v, "source"),
            "convention section path needs inline text or exact source-backed headings",
        )?;
    }
    if name == "ConventionHeadingSpan" {
        require(
            v["level"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|n| (1..=3).contains(&n)),
            "retained headings are levels 1..3",
        )?;
    }
    if name == "ConventionPathSource" {
        let spans = v["headings"].as_array().ok_or("heading spans are absent")?;
        require(
            spans.len() <= 3,
            "path has at most three captured ancestor headings",
        )?;
        let limit = v["artifact"]["byteLength"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("source length absent")?;
        let mut previous_level = 0;
        for span in spans {
            let level = span["level"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("heading level absent")?;
            let start = span["firstByte"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("heading start absent")?;
            let count = span["byteLength"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("heading length absent")?;
            require(
                level > previous_level && start.checked_add(count).is_some_and(|end| end <= limit),
                "heading ancestors/ranges disagree with exact source",
            )?;
            previous_level = level;
        }
    }
    if name == "ConventionResourceRecord" {
        require(
            v["mediaType"]
                .as_str()
                .is_some_and(|s| s.chars().count() <= 200 && !s.contains('\0')),
            "invalid retained resource media type",
        )?;
        require(text("name", 255), "invalid convention resource name")?;
        require(
            (v["lifecycle"] == "Deleted") == present(v, "deletedAtMs"),
            "resource deletion state disagrees with its timestamp",
        )?;
    }
    if name == "ConventionIndexStatus" && (v["status"] == "Ready" || v["status"] == "LexicalOnly") {
        for key in [
            "generationId",
            "sourceRevision",
            "sourceDigest",
            "indexedChunks",
        ] {
            require(
                present(v, key),
                "published index needs observed source and counts",
            )?;
        }
        require(
            (v["status"] == "Ready") == present(v, "embeddingSpace"),
            "index status must describe actual vector coverage",
        )?;
    }
    if name == "ConventionResourcesSelection" && v["kind"] == "Document" && present(v, "maxChars") {
        require(
            v["maxChars"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|n| (1..=65536).contains(&n)),
            "document passage limit is 1..65536",
        )?;
    }
    if name == "ConventionPassage" {
        require(
            v["text"].as_str().is_some_and(|text| text.len() <= 65_536),
            "convention passage exceeds its UTF-8 byte ceiling",
        )?;
    }
    if matches!(
        name,
        "ConventionPassage"
            | "ConventionBinaryPassage"
            | "ConventionPage"
            | "ConventionResourcePage"
            | "ConventionSearchPage"
    ) {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "convention continuation mismatch",
        )?;
    }
    if name == "ConventionBinaryPassage" {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "resource continuation mismatch",
        )?;
    }
    if name == "ConventionReadView" {
        require(
            v["format"] != "Resource",
            "resource bytes use their dedicated byte-page outcome",
        )?;
        require(
            (v["availability"] == "Available") == present(v, "passage"),
            "available convention read needs an actual passage",
        )?;
        require(
            v["availability"] == "Available" || present(v, "reason"),
            "unavailable convention body needs its reason",
        )?;
    }
    if name == "ConventionRetrieval" {
        require(
            present(v, "embeddingRequestId") == present(v, "embeddingOperation"),
            "embedding custody needs its actual request and preparation operation",
        )?;
        require(
            (v["mode"] == "Hybrid") == present(v, "embeddingSpace"),
            "hybrid retrieval needs an actual compatible embedding space",
        )?;
        require(
            v["mode"] == "Hybrid" || present(v, "reason"),
            "nonhybrid retrieval needs its reason",
        )?;
    }
    Ok(())
}

fn convention_input(op: &str, v: &Value) -> Result<(), String> {
    if op == "conventions.read" {
        require(
            ["conventionId", "slug", "operationId", "path"]
                .iter()
                .filter(|key| present(v, key))
                .count()
                == 1,
            "read requires one convention, scoped slug, exact path or actual operation",
        )?;
        require(
            !(present(v, "slug") || present(v, "path")) || present(v, "scope"),
            "slug or path resolution requires its exact scope",
        )?;
        if present(v, "path") {
            require(
                v["path"].as_array().is_some_and(|parts| {
                    (1..=4).contains(&parts.len())
                        && parts
                            .iter()
                            .all(|part| part.as_str().is_some_and(|s| s.chars().count() <= 200))
                }),
                "exact convention path needs 1..4 literal components of at most 200 characters",
            )?;
        }
        require(
            (v["format"] == "SectionPath") == present(v, "sectionPath"),
            "section path read requires its exact generation and chunk",
        )?;
        if present(v, "maxChars") {
            require(
                v["maxChars"]
                    .as_str()
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|n| (1..=65536).contains(&n)),
                "convention passage limit is 1..65536 characters",
            )?;
        }
    }
    if op == "conventions.read" && present(v, "operationId") {
        require(
            [
                "revision",
                "format",
                "cursor",
                "maxChars",
                "sectionPath",
                "resourceId",
                "maxBytes",
            ]
            .iter()
            .all(|key| !present(v, key)),
            "operation status cannot select document bytes",
        )?;
    }
    if op == "conventions.retire" {
        require(
            present(v, "conventionId") != present(v, "operationId"),
            "retire requires one convention or actual operation",
        )?;
        require(
            !present(v, "operationId") || !present(v, "resourceId"),
            "operation cancellation cannot select a resource",
        )?;
    }
    if op == "conventions.search" && present(v, "topK") {
        require(
            v["topK"]
                .as_str()
                .and_then(|s| s.parse::<i32>().ok())
                .is_some(),
            "topK must fit the retained signed integer",
        )?;
    }
    if op == "conventions.read" {
        require(
            (v["format"] == "Resource") == present(v, "resourceId"),
            "resource read requires its exact owning resource",
        )?;
        require(
            !present(v, "maxBytes") || v["format"] == "Resource",
            "byte limit applies only to resource bytes",
        )?;
        require(
            v["format"] != "Resource" || !present(v, "maxChars"),
            "resource byte pages cannot select character limits",
        )?;
        if present(v, "maxBytes") {
            require(
                v["maxBytes"]
                    .as_str()
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_some_and(|n| (1..=49152).contains(&n)),
                "resource page is 1..49152 bytes",
            )?;
        }
    }
    if op == "conventions.index" {
        require(
            present(v, "conventionId") != present(v, "allScope"),
            "index requires one convention or a current scoped catalog",
        )?;
        require(
            !present(v, "allScope") || (!present(v, "resourceId") && !present(v, "scope")),
            "scoped indexing cannot select one resource",
        )?;
    }
    if op == "conventions.applicability" {
        require(
            v["lifecycle"] != "Deleted",
            "delete requires its dedicated operation",
        )?;
    }
    for key in ["title", "slug"] {
        if op == "conventions.edit"
            && let Some(s) = v[key].as_str()
        {
            let max = if key == "title" { 500 } else { 200 };
            require(
                (key == "slug" || !s.trim().is_empty()) && s.chars().count() <= max,
                "invalid convention metadata",
            )?;
        }
    }
    if present(v, "sortOrder") {
        require(
            v["sortOrder"]
                .as_str()
                .and_then(|s| s.parse::<i32>().ok())
                .is_some(),
            "invalid convention order",
        )?;
    }
    if let Some(resources) = v["resources"].as_array() {
        let mut changed = std::collections::HashSet::new();
        for row in resources {
            if let Some(id) = row["resourceId"].as_str() {
                require(changed.insert(id), "duplicate resource change")?;
            }
            if let Some(name) = row["name"].as_str() {
                require(
                    !name.trim().is_empty() && name.chars().count() <= 255,
                    "invalid resource name",
                )?;
            }
            if let Some(media) = row["mediaType"].as_str() {
                require(
                    media.chars().count() <= 200 && !media.contains('\0'),
                    "invalid original resource media type",
                )?;
            }
            require(
                row["lifecycle"] != "Deleted",
                "resource deletion requires its dedicated operation",
            )?;
        }
    }
    Ok(())
}

pub fn record(name: &str, v: &Value) -> Result<(), String> {
    if name == "WatchTimer" {
        require(
            present(v, "atMs") != present(v, "afterMs"),
            "a timer names exactly one of a time and a delay",
        )?;
    }
    if name == "WatchProbe" {
        require(
            v["intervalMs"]
                .as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .is_some_and(|ms| ms >= 1000),
            "a probe runs at most once a second",
        )?;
    }
    if name == "EmbeddingSpaceRecord" {
        require(
            v["dimensions"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|n| (1..=65536).contains(&n)),
            "embedding dimensions are 1..65536",
        )?;
        if v["status"] == "Qualified" {
            require(
                [
                    "qualificationOperation",
                    "qualificationRequestId",
                    "qualificationAttemptId",
                    "resultArtifact",
                ]
                .iter()
                .all(|key| present(v, key)),
                "qualification requires actual native evidence",
            )?;
        }
    }
    if name == "EmbeddingSpacePage" {
        require(
            v["qualificationAvailable"] == true || present(v, "availabilityReason"),
            "qualification unavailability needs its actual reason",
        )?;
        require(
            v["hasMore"] == present(v, "nextCursor")
                && present(v, "selectedSpaceId") == present(v, "selectedCandidateId"),
            "embedding page continuation/selection mismatch",
        )?;
    }

    convention_record(name, v)?;
    if name.starts_with("Code") {
        code_record(name, v)?;
    }

    if name == "WorkRecord" {
        require(
            v["kind"] == "Feature" || !present(v, "budget"),
            "only features have spending limits",
        )?;
    }
    if matches!(
        name,
        "UsageAttemptPage" | "UsageAggregatePage" | "UsageFilterOptions" | "PriceCatalogView"
    ) {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "usage or price page continuation mismatch",
        )?;
    }
    if name == "UsageAttemptPage" {
        require(
            v["context"]["reportKind"] == "Attempts",
            "attempt page requires Attempts selection",
        )?;
    }
    if name == "UsageFilterOptions" {
        require(
            v["context"]["reportKind"] == "Filters",
            "filter page requires Filters selection",
        )?;
    }
    if name == "UsageAggregatePage" {
        require(
            !matches!(
                v["context"]["reportKind"].as_str(),
                Some("Attempts" | "Filters")
            ),
            "aggregate page requires a report selection",
        )?;
    }
    if name == "UsageAttemptRecord"
        && let Some(quantities) = v["unitQuantities"].as_array()
    {
        require(
            quantities.len() <= 128,
            "measured usage quantities exceed host bound",
        )?;
        let mut dimensions = std::collections::HashSet::new();
        require(
            quantities.iter().all(|q| {
                dimensions.insert((
                    q["modality"].as_str(),
                    q["chargeKind"].as_str(),
                    q["unit"].as_str(),
                    q["cacheWriteTtlMs"].as_str(),
                ))
            }),
            "duplicate measured usage dimension/TTL",
        )?;
    }
    if name == "UsageReportRange" {
        let start = v["startMs"].as_str().and_then(|n| n.parse::<i128>().ok());
        let end = v["endMs"].as_str().and_then(|n| n.parse::<i128>().ok());
        require(
            matches!((start, end), (Some(a), Some(b)) if a < b),
            "usage range requires start before exclusive end",
        )?;
    }
    if name == "UsageQueryContext" {
        require(
            matches!(v["interval"].as_str(), Some("day" | "week" | "month")),
            "usage output interval is normalized",
        )?;
    }
    if name == "PriceTariff"
        && present(v, "contextLowerExclusive")
        && present(v, "contextUpperInclusive")
    {
        let lower = v["contextLowerExclusive"]
            .as_str()
            .and_then(|n| n.parse::<u128>().ok());
        let upper = v["contextUpperInclusive"]
            .as_str()
            .and_then(|n| n.parse::<u128>().ok());
        require(
            matches!((lower, upper), (Some(a), Some(b)) if a < b),
            "tariff context tier is empty",
        )?;
    }
    if name == "UsageCostSnapshot" {
        require(
            v["monetaryComplete"] != true || present(v, "amountMicroUsd"),
            "complete monetary coverage requires a known amount",
        )?;
        require(
            v["monetaryComplete"] == true || present(v, "untrackedReason"),
            "partial monetary coverage requires its reason",
        )?;
        if matches!(v["priceKind"].as_str(), Some("Unknown" | "Untracked")) {
            require(
                v["monetaryComplete"] == false,
                "unknown or untracked cost cannot claim complete coverage",
            )?;
        }
        if v["priceKind"] == "Free" {
            require(
                v["amountMicroUsd"] == "0" && v["priceOrigin"] == "LocalFree",
                "free cost needs deliberate local zero classification",
            )?;
        }
    }
    if name == "UsageTotals" {
        let n = |key: &str| {
            v[key]
                .as_str()
                .and_then(|n| n.parse::<u128>().ok())
                .unwrap_or(0)
        };
        require(
            n("trackedMicroUsd") == n("reportedMicroUsd") + n("estimatedMicroUsd"),
            "tracked totals sum reported and estimated cost only",
        )?;
        for key in [
            "localFreeAttempts",
            "untrackedAttempts",
            "unknownTailAttempts",
            "incompleteAttempts",
            "monetaryIncompleteAttempts",
        ] {
            require(
                n(key) <= n("attempts"),
                "usage coverage exceeds selected attempts",
            )?;
        }
    }

    if matches!(
        name,
        "WorkRecord"
            | "TaskPatch"
            | "IssuePatch"
            | "TaskPlanEntry"
            | "FeaturePatch"
            | "FeaturePlanDraft"
    ) {
        work_metadata(v)?;
    }
    if name == "WorkRecord" {
        require(
            v["kind"] != "Feature" || !present(v, "taskType"),
            "feature cannot have a card type",
        )?;
    }
    if name == "KnowledgeReviewItem" {
        require(
            v["informational"] == true,
            "Knowledge review candidates are informational",
        )?;
        require(
            v["challenges"]
                .as_array()
                .is_none_or(|items| items.len() <= 3),
            "review shows at most three open challenges",
        )?;
    }
    if name == "KnowledgeReviewPage" {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "review page continuation mismatch",
        )?;
        require(
            v["items"].as_array().is_none_or(|items| items.len() <= 50),
            "review page limit is fifty",
        )?;
    }
    if name == "ApprovalViewRecord" && v["payload"]["kind"] == "SavedOperation" {
        let v = &v["payload"];
        require(
            v["schemaRevision"]
                .as_str()
                .and_then(|n| n.parse::<u16>().ok())
                .is_some(),
            "saved operation schema revision exceeds supported range",
        )?;
        require(
            v["input"].is_object() && v["input"].get("kind").is_none(),
            "saved operation input is the closed argument object without operation kind",
        )?;
    }
    if name == "HelpSnapshot" {
        require(
            v["canGenerate"] == !present(v, "cannotGenerateReason"),
            "Help generation permission needs an exact refusal reason",
        )?;
    }
    if name == "HelpCapabilityMetadata" {
        require(
            v["available"] == !present(v, "unavailableReason"),
            "Help capability availability needs an exact reason",
        )?;
    }
    if matches!(name, "HelpPassage" | "HelpTopicPage") {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "Help continuation mismatch",
        )?;
    }
    if matches!(name, "HelpTopicSummary" | "HelpTopicPage") && v["availability"] != "Available" {
        require(present(v, "reason"), "unavailable Help needs a reason")?;
    }
    if name == "HelpReadView" {
        match v["kind"].as_str() {
            Some("Available") => require(
                v["topic"]["availability"] == "Available",
                "available Help read needs an available topic",
            )?,
            Some("Unavailable") => require(
                v["availability"] != "Available",
                "unavailable Help cannot claim availability",
            )?,
            _ => {}
        }
    }
    eval_record(name, v)?;
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
        "DependencyEdge" => require(
            v["predecessor"] != v["successor"],
            "task cannot block itself",
        )?,
        "DependencyReplacement" => require(unique(v, "edges"), "duplicate blocking link")?,
        "PendingFeaturePlan" => {
            if present(v, "approval") {
                require(
                    v["approval"]["state"] != "Approved",
                    "approved draft must be promoted, not remain pending",
                )?;
                require(
                    v["approval"]["planRevision"] == v["revision"],
                    "pending approval must match the current draft",
                )?;
            }
        }
        "WorkRecord" => {
            require(
                v["kind"] != "Feature" || !present(v, "parentId"),
                "feature cannot have feature membership",
            )?;
            require(
                !present(v, "pendingPlan") || v["kind"] == "Feature",
                "pending feature plan belongs to a feature",
            )?;
            if v["kind"] == "Feature" && present(v, "approval") {
                require(
                    present(v, "plan") && v["approval"]["state"] == "Approved",
                    "active feature approval needs its approved plan",
                )?;
            }
        }
        "ReviewSourceEvidence" if v["state"] == "Completed" => {
            require(
                present(v, "completedAt")
                    && v["findingsCoverage"]["state"] == "Complete"
                    && v["findingsCoverage"]["moreGaps"] == false
                    && v["findingsCoverage"]["gaps"]
                        .as_array()
                        .is_some_and(|a| a.is_empty()),
                "review completion needs terminal time and complete findings coverage",
            )?;
            require(
                v["findingCount"] != "0" || v["zeroFindingsConfirmed"] == true,
                "zero findings need explicit completion evidence",
            )?;
            require(
                v["findingCount"] == "0" || v["zeroFindingsConfirmed"] == false,
                "nonzero findings cannot claim zero-findings completion",
            )?;
        }
        "ReviewRecord" if v["state"] == "Ready" => {
            require(
                v["sources"].as_array().is_some_and(|sources| {
                    sources
                        .iter()
                        .any(|s| s["state"] == "Completed" && s["headDigest"] == v["headDigest"])
                }),
                "ready review needs current-head completed source evidence",
            )?;
        }
        "RequirementItem" => require(
            v["status"] != "Dropped" || present(v, "dropDecision"),
            "dropped requirement needs decision",
        )?,
        "EnvironmentEntry" => require(
            present(v, "value") != present(v, "credentialRef"),
            "environment needs value or credential reference",
        )?,
        "PurgedReceiptOutcome" => {
            require(
                v["originalDecision"] != "Accepted" || present(v, "operationId"),
                "accepted purged receipt needs its original operation",
            )?;
            require(
                v["originalDecision"] == "Refused" || !present(v, "originalFaultCode"),
                "only an originally refused receipt carries a fault code",
            )?;
        }
        "ProjectPurgeEntryPage" => require(
            v["hasMore"] == present(v, "nextCursor"),
            "purge page continuation mismatch",
        )?,
        "ProjectPurgeManifest" => {
            require(
                v["phase"] != "Complete" || v["pendingEffects"] == "0",
                "complete purge has no pending effects",
            )?;
            require(
                v["phase"] != "Complete" || !present(v, "errorCode"),
                "complete purge has no unresolved failure",
            )?;
            require(
                v["phase"] != "Held" || present(v, "errorCode"),
                "held purge needs its actual failure code",
            )?;
            require(
                !matches!(
                    v["phase"].as_str(),
                    Some("RemovingBytes" | "Held" | "Complete")
                ) || present(v, "operationId"),
                "committed purge needs durable operation identity",
            )?;
        }
        "ReceiptObservation" => {
            let recorded = v["state"] == "Recorded";
            let purged = present(v, "purged");
            require(
                !purged || recorded,
                "only an originally committed receipt can be purged",
            )?;
            require(
                !recorded
                    || ["commandId", "committedSequence"]
                        .iter()
                        .all(|k| present(v, k)),
                "recorded receipt needs its committed identity",
            )?;
            require(
                !recorded
                    || purged
                    || ["replyArtifact", "replySchema"]
                        .iter()
                        .all(|k| present(v, k)),
                "recorded receipt needs durable typed reply or explicit purged outcome",
            )?;
            require(
                recorded && !purged || !present(v, "replyArtifact") && !present(v, "replySchema"),
                "unrecorded/purged receipt cannot claim reply content",
            )?;
            if purged {
                for key in ["commandId", "committedSequence", "operationId"] {
                    require(
                        v.get(key) == v["purged"].get(key),
                        "purged receipt identity mismatch",
                    )?;
                }
            }
        }
        "DiagnosticPage" => require(
            v["hasMore"] == present(v, "nextCursor"),
            "page continuation mismatch",
        )?,
        "DiagnosticMetric" => {
            let available = v["availability"] == "Available";
            require(
                present(v, "value") == available,
                "a metric value is present exactly when the metric is available",
            )?;
            require(
                available || present(v, "reason"),
                "an unavailable metric says why",
            )?;
        }
        "DiagnosticEntry" => {
            require(
                ordered(v, "startedAtMs", "finishedAtMs"),
                "a diagnostic cannot finish before it started",
            )?;
            require(
                ordered(v, "progressCompleted", "progressTotal"),
                "completed progress cannot exceed its total",
            )?;
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
        // A recorded redirect is a 3xx answer; a fetched document's final answer is 2xx.
        "WebRedirect" => require(
            v["status"]
                .as_i64()
                .is_some_and(|s| (300..=399).contains(&s)),
            "a redirect hop is a 3xx answer",
        )?,
        "WebDocument" => require(
            v.get("status")
                .is_none_or(|s| s.as_i64().is_some_and(|s| (200..=299).contains(&s))),
            "a fetched document is a 2xx answer",
        )?,
        "DecisionQuestion" => {}
        _ => {}
    }
    Ok(())
}
pub fn input(op: &str, mode: &str, v: &Value) -> Result<(), String> {
    if op == "projects.purge" {
        require(
            v["confirm"] == true,
            "permanent purge needs explicit confirmation",
        )?;
    }

    if op == "router.configure" && v["action"]["kind"] == "CreateEmbeddingSpace" {
        require(
            v["action"]["dimensions"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|n| (1..=65536).contains(&n)),
            "embedding dimensions are 1..65536",
        )?;
    }

    if op.starts_with("conventions.") {
        convention_input(op, v)?;
    }
    if op == "repositories.nodes" {
        require(
            present(v, "views") != present(v, "current")
                && v.get("current").is_none_or(|v| v == true),
            "repository nodes requires explicit or current views",
        )?;
    }

    if op == "help.read" && present(v, "maxChars") {
        require(
            v["maxChars"]
                .as_str()
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| (1..=65536).contains(&n)),
            "Help passage limit is 1..65536 characters",
        )?;
    }
    if op == "help.topic_for_screen" {
        require(
            present(v, "capabilityRevision") != present(v, "current"),
            "Help screen query needs exactly one revision selector",
        )?;
    }
    if op == "help.topic_for_screen" && present(v, "current") {
        require(v["current"] == true, "Help current selector must be true")?;
    }

    if op == "features.edit_plan" && present(v, "clearBudgetProposal") {
        require(
            v["clearBudgetProposal"] == true && !present(v, "budget"),
            "clearBudgetProposal must be true and excludes budget",
        )?;
    }
    if op == "router.price_override" {
        require(
            present(v, "price") != present(v, "change"),
            "override requires exactly one legacy price or explicit change",
        )?;
    }
    if op == "router.usage" {
        if let Some(interval) = v["interval"].as_str() {
            require(
                matches!(
                    interval.trim().to_ascii_lowercase().as_str(),
                    "" | "auto" | "day" | "week" | "month"
                ),
                "usage interval must be auto/day/week/month",
            )?;
        }
        for key in ["user", "provider", "model", "tool"] {
            require(
                v[key]
                    .as_str()
                    .is_none_or(|s| s.trim().chars().count() <= 65536),
                "usage filter exceeds its output bound",
            )?;
        }
        if present(v, "startMs") && present(v, "endMs") {
            let start = v["startMs"].as_str().and_then(|n| n.parse::<i128>().ok());
            let end = v["endMs"].as_str().and_then(|n| n.parse::<i128>().ok());
            require(
                matches!((start, end), (Some(a), Some(b)) if a < b),
                "usage start must precede exclusive end",
            )?;
        }
    }

    if op == "approvals.propose" && v["payload"]["kind"] == "SavedOperation" {
        let saved = &v["payload"];
        require(
            saved["schemaRevision"]
                .as_str()
                .and_then(|n| n.parse::<u16>().ok())
                .is_some(),
            "saved operation schema revision exceeds supported range",
        )?;
        require(
            saved["input"].is_object() && saved["input"].get("kind").is_none(),
            "saved operation input is the closed argument object without operation kind",
        )?;
    }
    if op == "approvals.review_queue" {
        require(
            v["page"]["limit"].as_u64().is_none_or(|limit| limit <= 50),
            "review queue limit is fifty",
        )?;
    }
    if matches!(op, "tasks.create" | "issues.create") {
        work_metadata(v)?;
    }
    if op == "features.create" && v["creation"]["kind"] == "Single" {
        work_metadata(&v["creation"])?;
    }
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
    if op == "knowledge.delete_by_source" {
        require(
            present(v, "sourceId") != present(v, "sourceLabel"),
            "cleanup names exactly one source selector",
        )?;
    }
    if op == "knowledge.diagnostics" {
        require(
            !(present(v, "sourceId") && present(v, "sourceLabel")),
            "cleanup names at most one source selector",
        )?;
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
    if op == "watch.create" {
        let sources = ["process", "probe", "timer"]
            .iter()
            .filter(|k| present(v, k))
            .count();
        require(sources == 1, "a watcher names exactly one source")?;
        require(
            !present(v, "maxLifetimeMs") || present(v, "probe"),
            "only a probe watcher takes a maximum lifetime",
        )?;
        require(
            !present(v, "tailBytes") || !present(v, "timer"),
            "a timer watcher has no output tail",
        )?;
    }
    if op == "indexing.status" {
        // Exactly one target: a repository, or (revision 2) one overlay.
        require(
            present(v, "repositoryId") != present(v, "overlayId"),
            "status names exactly one of a repository and an overlay",
        )?
    }
    if op == "indexing.runs" {
        require(
            unique(v, "states") && unique(v, "kinds"),
            "a run filter names each state and kind once",
        )?
    }
    if matches!(
        op,
        "tasks.prioritize" | "features.prioritize" | "issues.prioritize"
    ) {
        // Exactly one order: the ordered IDs, or (revision 2) one placement.
        let ids = if op == "tasks.prioritize" {
            "orderedTaskIds"
        } else {
            "orderedWorkIds"
        };
        require(
            present(v, ids) != present(v, "placement"),
            "a prioritization names exactly one of ordered IDs and a placement",
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
                require(
                    edges.iter().all(|edge| edge["successor"] == v["taskId"]),
                    "replacement may only change target task prerequisites",
                )?;
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
            let (Some(ids), Some(rs)) =
                (v["claimIds"].as_array(), v["expectedRevisions"].as_array())
            else {
                return Err("tag revisions must match selected claims".into());
            };
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
            let Some(a) = v["questions"].as_array() else {
                return Err("decision questions missing".into());
            };
            let mut seen = std::collections::HashSet::new();
            for q in a {
                require(
                    seen.insert(q["name"].to_string()),
                    "duplicate decision question",
                )?;
                let question = &q["question"];
                if question["kind"] == "Choice" {
                    let mut names = std::collections::HashSet::new();
                    for criterion in question["criteria"].as_array().into_iter().flatten() {
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
    if (op == "settings.save_revision" || op == "settings.validate")
        && let Some(entries) = v["entries"].as_array()
    {
        for entry in entries {
            let key = entry["key"].as_str().unwrap_or("");
            if key == "github.issueTriageIntervalSeconds" || key == "reviews.automationWaitSeconds"
            {
                require(
                    entry["value"]["value"]
                        .as_u64()
                        .is_some_and(|n| n > 0 && n <= i64::MAX as u64),
                    "interval must be a positive signed-64 integer",
                )?;
                if key == "github.issueTriageIntervalSeconds" {
                    require(
                        v["scope"]["kind"] == "Workspace",
                        "triage interval is app-wide only",
                    )?;
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
            "code.source" | "code.snippet" => matches!(kind, "Source" | "SourceText"),
            "code.graph_schema" => matches!(kind, "Schema" | "SchemaDetails"),
            "code.graph_scope_status" => matches!(kind, "ScopeStatus" | "ScopeDetails"),
            "code.search" => matches!(kind, "Nodes" | "Graph" | "CodeSearch"),
            "code.architecture" => matches!(kind, "Nodes" | "Graph" | "Architecture"),
            "code.overview" => matches!(kind, "Nodes" | "Graph" | "RepositoryOverview"),
            "code.nodes_by_file" | "repositories.nodes" => {
                matches!(kind, "Nodes" | "Graph" | "CodeNodes")
            }
            "code.node_detail" => matches!(kind, "Nodes" | "Graph" | "NodeDetail"),
            "code.impact" | "code.file_impact" => matches!(kind, "Nodes" | "Graph" | "Impact"),
            "code.call_path" => matches!(kind, "Nodes" | "Graph" | "CallTrace"),
            "code.data_lineage" => matches!(kind, "Nodes" | "Graph" | "Lineage"),
            "code.consumers" | "code.publishers" => matches!(kind, "Nodes" | "Graph" | "Related"),
            "code.repository_dependencies" => matches!(kind, "Nodes" | "Graph" | "Dependencies"),
            _ => matches!(kind, "Nodes" | "Graph" | "CodeGraph"),
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
            require(
                kind == expected
                    || expected == "TaskOrIssue" && (kind == "Task" || kind == "Issue"),
                "work kind does not match operation",
            )?
        }
        if let Some(items) = value["items"].as_array() {
            for x in items {
                if let Some(kind) = x.get("kind") {
                    require(
                        kind == expected
                            || expected == "TaskOrIssue" && (kind == "Task" || kind == "Issue"),
                        "work list kind does not match operation",
                    )?
                }
            }
        }
    }
    Ok(())
}

/// Evaluation arithmetic preserves ungraded outcomes instead of folding them into a pass rate.
fn eval_record(name: &str, v: &Value) -> Result<(), String> {
    if name == "EvalCoverage" {
        let count = |k: &str| {
            v[k].as_str()
                .and_then(|s| s.parse::<u128>().ok())
                .ok_or_else(|| format!("invalid eval count: {k}"))
        };
        let eligible = count("eligible")?;
        let graded = count("graded")?;
        let ungraded = count("ungraded")?;
        let passed = count("passed")?;
        let failed = count("failed")?;
        require(
            eligible == graded + ungraded && graded == passed + failed,
            "eval coverage counts disagree",
        )?;
        let reasons = v["ungradedReasons"]
            .as_array()
            .ok_or("missing eval ungraded reasons")?;
        let mut sum = 0_u128;
        let mut seen = std::collections::HashSet::new();
        for reason in reasons {
            let outcome = reason["outcome"]
                .as_str()
                .ok_or("missing ungraded outcome")?;
            require(
                !matches!(outcome, "passed" | "failed") && seen.insert(outcome),
                "graded or duplicate outcome in ungraded reasons",
            )?;
            let n = reason["count"]
                .as_str()
                .and_then(|s| s.parse::<u128>().ok())
                .ok_or("invalid ungraded reason count")?;
            require(n > 0, "zero ungraded reason count")?;
            sum += n;
        }
        require(sum == ungraded, "ungraded reason counts disagree")?;
        let rate = v
            .get("passRate")
            .filter(|x| !x.is_null())
            .and_then(Value::as_f64);
        require(
            if graded == 0 {
                rate.is_none()
            } else {
                rate.is_some_and(|x| (x - passed as f64 / graded as f64).abs() <= f64::EPSILON)
            },
            "eval pass rate must use only passed and failed",
        )?;
    }
    if name == "EvalDeterministicCheck" {
        require(
            v["measurement"] == "Proxy",
            "built-in deterministic predicates must declare proxy measurement",
        )?;
        require(
            v["predicate"]["kind"] != "TranscriptContains"
                || v["predicate"]["text"]
                    .as_str()
                    .is_some_and(|x| !x.is_empty()),
            "empty transcript match is not a measurement",
        )?;
    }
    if name == "EvalReportPage" {
        require(
            v["hasMore"] == present(v, "nextCursor"),
            "eval page continuation mismatch",
        )?;
    }
    if name == "EvalSuiteSnapshot" {
        require(
            serde_json::to_vec(v).map_err(|e| e.to_string())?.len() <= 64 * 1024,
            "eval suite snapshot exceeds 64 KiB",
        )?;
        let checks = v["checks"].as_array().ok_or("missing eval checks")?;
        let mut seen = std::collections::HashSet::new();
        require(
            checks.iter().all(|c| seen.insert(c["id"].to_string())),
            "duplicate eval check identity",
        )?;
    }
    Ok(())
}

fn work_metadata(v: &Value) -> Result<(), String> {
    require(
        !(present(v, "taskType") && v["clearTaskType"] == true),
        "task type cannot be set and cleared together",
    )?;
    if let Some(labels) = v["labels"].as_array() {
        require(unique(v, "labels"), "duplicate work label")?;
        require(
            labels
                .iter()
                .all(|s| s.as_str().is_some_and(|s| !s.trim().is_empty())),
            "empty work label",
        )?;
    }
    Ok(())
}
