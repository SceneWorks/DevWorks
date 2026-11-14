//! Real retained reporting over SQLite facts; no dispatch, provider calls or inferred ownership.
use super::{Harness, T0};
use crate::storage::sqlite::SqliteStore;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::operations::router::UsageInput;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::*;
use devworks_core::protocol::{Counter, Id, OperationId, Revision};
use devworks_core::services::usage::{
    accounting::{AdmittedSnapshot, BalanceSummary},
    ingest, reports,
};
use serde_json::{Value, json};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn read<T>(s: &SqliteStore, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
    read_snapshot(s, &ReadOptions::default(), f).unwrap()
}
fn seed(h: &Harness) -> SqliteStore {
    let s = h.store();
    let raw = h.raw();
    for project in ["project-a", "project-b"] {
        raw.execute("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES(?1,?1,'Active',1,1,?2)",rusqlite::params![project,T0]).unwrap();
    }
    raw.execute("INSERT INTO work_items(id,project_id,kind,title,priority,manual_order,revision,scope_revision,created_at,brief_json) VALUES('feature','project-a','feature','Feature',0,0,1,1,?1,'{}')",[T0]).unwrap();
    raw.execute("INSERT INTO work_items(id,project_id,kind,parent_feature_id,title,priority,manual_order,revision,scope_revision,created_at,brief_json) VALUES('issue','project-a','issue','feature','Issue',0,0,1,1,?1,'{}')",[T0]).unwrap();
    raw.execute("INSERT INTO router_connections(id,name,profile,endpoint,credential_revision,lifecycle,revision,created_at,updated_at) VALUES('connection','Actual transport','OpenAiResponses','https://example.test',0,'Disconnected',1,?1,?1)",[T0]).unwrap();
    drop(raw);
    run_write(&s, WriteOptions::default(), |tx| {
        while !ingest::bootstrap_page(tx, 64)?.complete {}
        Ok(())
    })
    .unwrap();
    s
}
fn ctx(s: &SqliteStore) -> InvocationContext {
    let info = s.info();
    HostIngress::new(s).context(
        info.host_actor,
        PrincipalKind::Host,
        id("report-invocation"),
        id("report-ingress"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}
fn input(kind: &str) -> UsageInput {
    serde_json::from_value(json!({"reportKind":kind,"startMs":(T0-1).to_string(),"endMs":(T0+86_400_000).to_string(),"page":{"limit":500}})).unwrap()
}
fn query(s: &SqliteStore, i: &UsageInput) -> Value {
    read(s, |r| {
        Ok(serde_json::to_value(reports::query(r, &ctx(s), i, T0 + 86_400_000).unwrap()).unwrap())
    })
}
fn request(h: &Harness, name: &str, category: &str, time: i64) {
    let raw = h.raw();
    let operation = format!("op-{name}");
    raw.execute("INSERT INTO operations(id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES(?1,'router.request',1,?2,?3,'Preflighted',1,?4)",rusqlite::params![operation,"a".repeat(64),json!({"reportCategory":category}).to_string(),time]).unwrap();
    raw.execute("INSERT INTO router_requests(request_id,operation_id,purpose,project_id,model_id,connection_id,created_at) VALUES(?1,?2,'Conversation','project-a','sol','connection',?3)",rusqlite::params![name,operation,time]).unwrap();
}
fn attempt(
    h: &Harness,
    s: &SqliteStore,
    name: &str,
    request: &str,
    number: i64,
    measurement: (Option<i64>, Option<i64>, i64),
) {
    let (input, output, amount) = measurement;
    let op = format!("op-{request}");
    h.raw().execute("INSERT INTO operation_attempts(id,operation_id,attempt_no,boot_id,state,observations_json) VALUES(?1,?2,?3,?4,'Preflighted','[]')",rusqlite::params![name,op,number,s.info().boot_id.unwrap().as_str()]).unwrap();
    let snapshot:AdmittedSnapshot=serde_json::from_value(json!({"admission_key":name,"intent_digest":"a".repeat(64),"quote":{"classification":"OfficialEstimate","connection_revision":1,"currency":"USD","rates":null,"tariffs":[]},"query":{"catalog_model_id":"sol","provider_model_id":"gpt-6.1-sol","connection_id":"connection","endpoint_profile":"OpenAiResponses","access_mode":"ApiKey","required_modalities":["Text"]},"price_generation":1,"override_revision":1,"pricing":{"input_semantics":"Inclusive","cache_read_possible":false,"cache_write_possible":false},"selection":{"Conversation":{"intent":{"kind":"Model","modelId":"sol"},"effort":"Default","required_capabilities":[]}},"admitted_boot_id":s.info().boot_id,"cache_diagnostics":[]})).unwrap();
    let mut summary = BalanceSummary::default();
    summary.meter.quantities.input_tokens = input;
    summary.meter.quantities.output_tokens = output;
    summary.outcome = Some("Completed".into());
    summary.finished_at_ms = Some(T0 + 100);
    let a = StoredInferenceAttempt {
        attempt_id: id(name),
        request_id: id(request),
        operation_id: OperationId(id(&op)),
        attempt_number: number,
        project_id: Some(devworks_core::protocol::ProjectId(id("project-a"))),
        feature_id: Some(id("feature")),
        work_id: Some(id("feature")),
        assignment_id: None,
        execution_root_id: None,
        execution_id: None,
        session_id: None,
        job_run_id: None,
        actor_id: s.info().host_actor,
        role: "CodeReviewer".into(),
        purpose: "Conversation".into(),
        catalog_model_id: id("sol"),
        connection_id: id("connection"),
        endpoint_profile: "OpenAiResponses".into(),
        access_mode: "ApiKey".into(),
        provider_model_id: Some("gpt-6.1-sol".into()),
        pricing_snapshot: serde_json::to_value(snapshot).unwrap(),
        state: "Admitted".into(),
        revision: 1,
        created_at_ms: T0,
    };
    let b = StoredAttemptBalance {
        attempt_id: id(name),
        revision: 1,
        priced_micro_usd: Some(amount),
        reserved_micro_usd: Some(10),
        classification: "OfficialEstimate".into(),
        monetary_complete: true,
        incomplete: false,
        unknown_tail: false,
        local_quiescent: true,
        summary: serde_json::to_value(summary).unwrap(),
    };
    run_write(s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage().put_attempt_balance(&b, None)?;
        Ok(())
    })
    .unwrap();
}
fn origin(
    s: &SqliteStore,
    name: &str,
    kind: &str,
    payload: Value,
    time: i64,
    amount: Option<i64>,
    request_id: Option<Id>,
) {
    let o = StoredUsageOrigin {
        source_kind: kind.into(),
        source_id: name.into(),
        attempt_id: None,
        request_id,
        project_id: Some(devworks_core::protocol::ProjectId(id("project-a"))),
        feature_id: None,
        work_id: None,
        session_id: None,
        job_run_id: None,
        payload,
        recorded_at_ms: time,
        priced_micro_usd: amount,
        classification: if amount.is_some() {
            "OfficialEstimate"
        } else {
            "Untracked"
        }
        .into(),
        monetary_complete: false,
        incomplete: false,
        unknown_tail: false,
        committed_sequence: (kind == "SessionUsage").then_some(1),
    };
    run_write(s, WriteOptions::default(), move |tx| {
        tx.usage().insert_usage_origin(&o)
    })
    .unwrap();
}
fn metric(v: &Value, key: &str) -> Option<i64> {
    v["report"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "Total" && r["key"] == key)
        .and_then(|r| r["value"].as_str())
        .and_then(|s| s.parse().ok())
}
fn gap(v: &Value, key: &str) -> bool {
    let page = if v["kind"] == "Attempts" {
        &v["page"]
    } else if v["kind"] == "Filters" {
        &v["filters"]
    } else {
        &v["report"]
    };
    page["coverage"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g == key)
}
#[test]
fn report_category_is_recorded_not_guessed_and_auxiliary_attempts_remain_visible() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "unknown", "Unknown", T0);
    attempt(
        &h,
        &s,
        "attempt-unknown",
        "unknown",
        1,
        (Some(5), Some(2), 20),
    );
    let v = query(&s, &input("CodeReviewUsage"));
    assert_eq!(metric(&v, "totalTokens"), None);
    assert!(
        gap(&v, "report-path-unrecorded"),
        "CodeReviewer role is not original service-path evidence"
    );
    let v = query(&s, &input("Attempts"));
    assert_eq!(v["page"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["page"]["totals"]["attempts"], "1");
    assert_eq!(v["page"]["items"][0]["purpose"], "Conversation");
    assert_eq!(v["page"]["items"][0]["actorId"], "host");
}
#[test]
fn assistant_tokens_count_attempts_while_activity_collapses_retries_and_includes_zero_attempt_requests()
 {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "logical", "Assistant", T0);
    attempt(&h, &s, "attempt-a", "logical", 1, (Some(10), Some(1), 100));
    attempt(&h, &s, "attempt-b", "logical", 2, (Some(20), Some(2), 200));
    request(&h, "zero", "Assistant", T0);
    origin(
        &s,
        "zero",
        "RouterRequest",
        json!({"version":1,"purpose":"Conversation","modelId":"sol","connectionId":"connection","operationId":"op-zero"}),
        T0,
        None,
        Some(id("zero")),
    );
    let v = query(&s, &input("AssistantUsage"));
    assert_eq!(metric(&v, "totalTokens"), Some(33));
    assert_eq!(metric(&v, "runCount"), Some(2));
    let v = query(&s, &input("AssistantActivity"));
    assert_eq!(metric(&v, "runCount"), Some(2));
    assert_eq!(
        metric(&v, "uniqueChats"),
        None,
        "missing actor/chat identity is not an invented unique chat"
    );
    let v = query(&s, &input("Attempts"));
    assert_eq!(v["page"]["totals"]["attempts"], "2");
    assert_eq!(v["page"]["totals"]["trackedMicroUsd"], "300");
    assert_eq!(v["page"]["totals"]["legacyUsageFacts"], "0");
    assert!(!gap(&v, "monetary-coverage-incomplete"));
}
#[test]
fn legacy_observations_are_retained_separately_without_an_invented_attempt_or_actor() {
    let h = Harness::new();
    let s = seed(&h);
    for n in 0..2 {
        origin(
            &s,
            &format!("observation-{n}"),
            "SessionUsage",
            json!({"version":1,"item":{"requestId":"request-old","purpose":"Compaction","modelId":"sol","inputTokens":"9","outputTokens":"1","cacheReadTokens":"0","cacheWriteTokens":"0","amountUsd":"0.2","priceKind":"Estimated","priceOrigin":"OfficialSnapshot","incomplete":false}}),
            T0,
            Some(200_000),
            Some(id("request-old")),
        );
    }
    let v = query(&s, &input("Attempts"));
    assert_eq!(v["page"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(v["page"]["totals"]["attempts"], "0");
    assert_eq!(v["page"]["totals"]["trackedMicroUsd"], "0");
    assert_eq!(v["page"]["totals"]["legacyObservationMicroUsd"], "400000");
    assert!(gap(&v, "legacy-observations-not-incurred-spend"));
    for row in v["page"]["items"].as_array().unwrap() {
        assert!(row.get("attemptId").is_none());
        assert!(row.get("actorId").is_none());
        assert!(row.get("providerId").is_none());
    }
}
#[test]
fn mcp_uses_positive_transport_provenance_ignores_model_filters_and_rounds_ties_even() {
    let h = Harness::new();
    let s = seed(&h);
    for (name, duration, outcome) in [("a", 0, "Completed"), ("b", 1, "Failed")] {
        origin(
            &s,
            name,
            "ToolInvocation",
            json!({"version":1,"capabilityId":"real-tool","transportOrigin":"Mcp","actorId":"human","outcome":outcome,"durationMs":duration}),
            T0,
            None,
            None,
        );
    }
    origin(
        &s,
        "not-proof",
        "ToolInvocation",
        json!({"version":1,"capabilityId":"mcp.named-but-unrecorded","actorId":"human","outcome":"Completed"}),
        T0,
        None,
        None,
    );
    let mut i = input("McpUsage");
    i.provider = Some(devworks_core::protocol::support::BoundedText(
        "ignored-provider".into(),
    ));
    i.model = Some(devworks_core::protocol::support::BoundedText(
        "ignored-model".into(),
    ));
    let v = query(&s, &i);
    assert_eq!(metric(&v, "callCount"), Some(2));
    assert_eq!(metric(&v, "successfulCalls"), Some(1));
    assert_eq!(metric(&v, "failedCalls"), Some(1));
    assert_eq!(metric(&v, "averageDurationMs"), Some(0));
    assert!(gap(&v, "tool-transport-unrecorded"));
    assert!(
        v["report"]["context"]["appliedFilters"]
            .get("provider")
            .is_none()
    );
    origin(
        &s,
        "missing-duration",
        "ToolInvocation",
        json!({"version":1,"capabilityId":"real-tool","transportOrigin":"Mcp","actorId":"human","outcome":"Completed"}),
        T0,
        None,
        None,
    );
    let v = query(&s, &i);
    assert_eq!(metric(&v, "callCount"), Some(3));
    assert_eq!(metric(&v, "averageDurationMs"), None);
    assert!(gap(&v, "report-metric-unavailable"));
}
#[test]
fn filter_options_ignore_categories_obey_range_and_use_utf16_ordinal_order() {
    let h = Harness::new();
    let s = seed(&h);
    for (name, tool, time) in [
        ("supplementary", "tool𐀀", T0),
        ("bmp", "tool\u{e000}", T0),
        ("outside", "outside", T0 - 2),
    ] {
        origin(
            &s,
            name,
            "ToolInvocation",
            json!({"version":1,"capabilityId":tool,"transportOrigin":"Mcp","actorId":" human ","outcome":"Completed","durationMs":2}),
            time,
            None,
            None,
        );
    }
    let mut i = input("Filters");
    i.user = Some(devworks_core::protocol::support::BoundedText(
        "excluded".into(),
    ));
    i.tool = Some(devworks_core::protocol::support::BoundedText(
        "excluded".into(),
    ));
    let v = query(&s, &i);
    let values = v["filters"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["dimension"] == "tool")
        .map(|r| r["value"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values, vec!["tool𐀀", "tool\u{e000}"]);
    assert!(
        v["filters"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["dimension"] == "user" && r["value"] == "human")
    );
    assert!(
        v["filters"]["context"]["appliedFilters"]
            .get("user")
            .is_none()
    );
}
#[test]
fn utc_half_open_range_missing_measurements_and_empty_buckets_are_distinct() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "known", "CodeReview", T0);
    attempt(&h, &s, "known-attempt", "known", 1, (Some(10), None, 100));
    let mut i = input("CodeReviewUsage");
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        (T0).to_string(),
    ));
    let empty = query(&s, &i);
    assert_eq!(metric(&empty, "totalTokens"), Some(0), "end is exclusive");
    i.start_ms = Some(devworks_core::protocol::support::SignedCounter(
        (T0).to_string(),
    ));
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        (T0 + 2 * 86_400_000).to_string(),
    ));
    let v = query(&s, &i);
    assert_eq!(metric(&v, "totalTokens"), None);
    assert_eq!(metric(&v, "inputTokens"), Some(10));
    assert!(gap(&v, "report-metric-unavailable"));
    assert!(
        v["report"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "SeriesPoint" && r["key"] == "totalTokens" && r["value"] == "0")
    );
}
#[test]
fn scopes_intersect_and_report_cursor_is_bound_to_range_view_limit_and_read_revision() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "known", "Assistant", T0);
    attempt(
        &h,
        &s,
        "known-attempt",
        "known",
        1,
        (Some(10), Some(2), 100),
    );
    let mut i = input("AssistantUsage");
    i.page.limit = serde_json::from_value(json!(2)).unwrap();
    let first = query(&s, &i);
    i.page.cursor = serde_json::from_value(first["report"]["nextCursor"].clone()).unwrap();
    let second = query(&s, &i);
    assert_ne!(first["report"]["items"], second["report"]["items"]);
    let mut changed = i.clone();
    changed.report_kind = Some(UsageReportKind::AssistantActivity);
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| Ok(reports::query(
            r,
            &ctx(&s),
            &changed,
            T0
        )
        .is_err()))
        .unwrap()
    );
    let project_ctx = ctx(&s).with_lineage(
        Some(devworks_core::protocol::ProjectId(id("project-a"))),
        None,
        None,
    );
    let mut other = input("Attempts");
    other.scope = Some(devworks_core::protocol::data::DataScope::Workspace);
    assert!(read(&s, |r| Ok(reports::query(
        r,
        &project_ctx,
        &other,
        T0
    )
    .is_err())));
    other.scope = Some(devworks_core::protocol::data::DataScope::Project {
        project_id: devworks_core::protocol::ProjectId(id("project-b")),
    });
    assert!(read(&s, |r| Ok(reports::query(
        r,
        &project_ctx,
        &other,
        T0
    )
    .is_err())));
    run_write(&s, WriteOptions::default(), |tx| {
        super::append(tx, "SourceChanged", json!({}))
    })
    .unwrap();
    assert!(read(&s, |r| Ok(
        reports::query(r, &ctx(&s), &i, T0).is_err()
    )));
}
#[test]
fn totals_read_every_source_page_and_remain_full_on_each_output_page() {
    let h = Harness::new();
    let s = seed(&h);
    for n in 0..501 {
        let request_id = format!("request-{n:04}");
        request(&h, &request_id, "Assistant", T0);
        attempt(
            &h,
            &s,
            &format!("attempt-{n:04}"),
            &request_id,
            1,
            (Some(1), Some(2), 1),
        );
    }
    let mut i = input("Attempts");
    i.page.limit = serde_json::from_value(json!(2)).unwrap();
    let first = query(&s, &i);
    assert_eq!(first["page"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(first["page"]["totals"]["attempts"], "501");
    assert_eq!(first["page"]["totals"]["trackedMicroUsd"], "501");
    i.page.cursor = serde_json::from_value(first["page"]["nextCursor"].clone()).unwrap();
    let next = query(&s, &i);
    assert_eq!(next["page"]["totals"], first["page"]["totals"]);
    assert_ne!(
        next["page"]["items"][0]["id"],
        first["page"]["items"][0]["id"]
    );
}
#[test]
fn served_model_is_captured_separately_from_catalog_identity_and_legacy_fallback_is_explicit() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "resolved", "Assistant", T0);
    attempt(
        &h,
        &s,
        "resolved-attempt",
        "resolved",
        1,
        (Some(4), Some(2), 3),
    );
    let mut i = input("AssistantUsage");
    i.model = Some(devworks_core::protocol::support::BoundedText(
        "gpt-6.1-sol".into(),
    ));
    let v = query(&s, &i);
    assert_eq!(metric(&v, "totalTokens"), Some(6));
    i.model = Some(devworks_core::protocol::support::BoundedText("sol".into()));
    assert_eq!(metric(&query(&s, &i), "totalTokens"), Some(0));
    let attempts = query(&s, &input("Attempts"));
    assert_eq!(attempts["page"]["items"][0]["modelId"], "sol");
    assert_eq!(
        attempts["page"]["items"][0]["resolvedModelId"],
        "gpt-6.1-sol"
    );
    origin(
        &s,
        "old-model",
        "SessionUsage",
        json!({"version":1,"item":{"purpose":"Title","modelId":"old-recorded-model","inputTokens":"2","outputTokens":"1","cacheReadTokens":"0","cacheWriteTokens":"0"}}),
        T0,
        None,
        None,
    );
    let v = query(&s, &input("Attempts"));
    let old = v["page"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["sourceKind"] == "SessionUsage")
        .unwrap();
    assert_eq!(old["modelId"], "old-recorded-model");
    assert!(old.get("requestId").is_none());
    assert!(old.get("actorId").is_none());
    assert!(gap(&v, "legacy-request-identity-unrecorded"));
    assert!(gap(&v, "actual-served-model-unrecorded"));
}
#[test]
fn monday_buckets_can_precede_epoch_and_auto_intervals_preserve_the_retained_thresholds() {
    let h = Harness::new();
    let s = seed(&h);
    let mut i = input("AssistantActivity");
    i.start_ms = Some(devworks_core::protocol::support::SignedCounter(
        (0).to_string(),
    ));
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        (86_400_000).to_string(),
    ));
    i.interval = Some(devworks_core::protocol::support::BoundedText(
        " WEEK ".into(),
    ));
    let v = query(&s, &i);
    assert_eq!(v["report"]["context"]["interval"], "week");
    assert!(
        v["report"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "SeriesPoint" && r["bucketStartMs"] == "-259200000")
    );
    for (days, expected) in [(45_i64, "day"), (46, "week"), (180, "week"), (181, "month")] {
        i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
            (days * 86_400_000).to_string(),
        ));
        i.interval = None;
        assert_eq!(query(&s, &i)["report"]["context"]["interval"], expected);
    }
    i.interval = Some(devworks_core::protocol::support::BoundedText("hour".into()));
    assert!(read(&s, |r| Ok(
        reports::query(r, &ctx(&s), &i, T0).is_err()
    )));
}
#[test]
fn an_omitted_default_range_is_frozen_into_the_cursor_and_epoch_changes_expire_it() {
    let h = Harness::new();
    let s = seed(&h);
    let mut i = input("AssistantActivity");
    i.start_ms = None;
    i.end_ms = None;
    i.page.limit = serde_json::from_value(json!(2)).unwrap();
    let first = read(&s, |r| {
        Ok(serde_json::to_value(reports::query(r, &ctx(&s), &i, T0).unwrap()).unwrap())
    });
    i.page.cursor = serde_json::from_value(first["report"]["nextCursor"].clone()).unwrap();
    let second = read(&s, |r| {
        Ok(serde_json::to_value(reports::query(r, &ctx(&s), &i, T0 + 99_000).unwrap()).unwrap())
    });
    assert_eq!(first["report"]["context"], second["report"]["context"]);
    assert_ne!(first["report"]["items"], second["report"]["items"]);
    let info = s.info();
    let stale = HostIngress::new(&s).context(
        info.host_actor,
        PrincipalKind::Host,
        id("other-invocation"),
        id("report-ingress"),
        Counter::new(info.restore_epoch + 1).unwrap(),
        Revision::new(1).unwrap(),
    );
    assert!(read(&s, |r| Ok(reports::query(r, &stale, &i, T0).is_err())));
}
#[test]
fn later_retries_do_not_move_a_logical_activity_out_of_its_original_start_cohort() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "old-start", "Assistant", T0 - 2);
    attempt(
        &h,
        &s,
        "old-start-attempt",
        "old-start",
        1,
        (Some(4), Some(1), 1),
    );
    assert_eq!(
        metric(&query(&s, &input("AssistantUsage")), "totalTokens"),
        Some(5)
    );
    assert_eq!(
        metric(&query(&s, &input("AssistantActivity")), "runCount"),
        Some(0),
        "usage occurred inside range but its logical activity began before it"
    );
    let mut i = input("AssistantActivity");
    i.start_ms = Some(devworks_core::protocol::support::SignedCounter(
        (T0 - 3).to_string(),
    ));
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        (T0 - 1).to_string(),
    ));
    assert_eq!(
        metric(&query(&s, &i), "runCount"),
        Some(1),
        "logical run remains in its start cohort even when attempt is outside the range"
    );
}
#[test]
fn long_labels_are_complete_stably_addressed_and_paged_under_the_actual_frame_byte_ceiling() {
    let h = Harness::new();
    let s = seed(&h);
    let labels = (0..8)
        .map(|n| format!("{n:02}-{}", "🙂".repeat(65533)))
        .collect::<Vec<_>>();
    for (n, label) in labels.iter().enumerate() {
        origin(
            &s,
            &format!("long-{n}"),
            "ToolInvocation",
            json!({"version":1,"capabilityId":label,"transportOrigin":"Mcp","actorId":"human","outcome":"Completed","durationMs":1}),
            T0,
            None,
            None,
        );
    }
    let mut i = input("McpUsage");
    let mut rows = Vec::new();
    let mut pages = 0;
    loop {
        let v = query(&s, &i);
        assert!(serde_json::to_vec(&v).unwrap().len() < devworks_core::framing::MAX_FRAME_BYTES);
        let page = v["report"]["items"].as_array().unwrap();
        assert!(!page.is_empty());
        if pages == 0 {
            assert!(page.len() < 500);
            assert_eq!(metric(&v, "callCount"), Some(8));
        }
        rows.extend(page.iter().cloned());
        pages += 1;
        if !v["report"]["hasMore"].as_bool().unwrap() {
            break;
        }
        i.page.cursor = serde_json::from_value(v["report"]["nextCursor"].clone()).unwrap();
    }
    assert!(
        pages > 1,
        "byte budget must page even when row-count limit is not reached"
    );
    let full = rows
        .iter()
        .filter(|r| r["kind"] == "Breakdown" && r["dimension"] == "tool")
        .map(|r| r["label"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(full.len(), labels.len());
    for label in &labels {
        assert!(full.contains(label));
    }
    for row in rows.iter().filter(|r| {
        r["kind"] == "SeriesPoint" && r["key"].as_str().unwrap().starts_with("breakdown:tool:")
    }) {
        assert!(row["key"].as_str().unwrap().chars().count() <= 256);
        assert!(labels.iter().any(|label| row["label"] == label.as_str()));
    }
}
#[test]
fn contradictory_cache_measurements_remain_observable_without_a_fabricated_uncached_count() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "cache", "Assistant", T0);
    attempt(
        &h,
        &s,
        "cache-attempt",
        "cache",
        1,
        (Some(20), Some(1), 100),
    );
    run_write(&s, WriteOptions::default(), |tx| {
        let mut b = tx.usage().attempt_balance(&id("cache-attempt"))?.unwrap();
        let mut summary: BalanceSummary = serde_json::from_value(b.summary).unwrap();
        summary.meter.quantities.cached_read_tokens = Some(10);
        b.summary = serde_json::to_value(summary).unwrap();
        b.revision += 1;
        tx.usage().put_attempt_balance(&b, Some(1))
    })
    .unwrap();
    let v = query(&s, &input("Attempts"));
    let row = &v["page"]["items"][0];
    assert_eq!(row["cacheReadTokens"], "10");
    assert!(row.get("uncachedInputTokens").is_none());
    assert!(row.get("cacheReadMicroUsd").is_none());
    assert!(gap(&v, "cache-profile-observation-conflict"));
}
#[test]
fn a_legal_early_end_preserves_the_exact_negative_thirty_day_default_start() {
    let h = Harness::new();
    let s = seed(&h);
    let mut i = input("AssistantActivity");
    i.start_ms = None;
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        (0).to_string(),
    ));
    let v = query(&s, &i);
    assert_eq!(v["report"]["context"]["range"]["startMs"], "-2592000000");
    assert_eq!(v["report"]["context"]["range"]["endMs"], "0");
    assert_eq!(metric(&v, "runCount"), Some(0));
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        (86_400_000).to_string(),
    ));
    let v = query(&s, &i);
    assert_eq!(v["report"]["context"]["range"]["startMs"], "-2505600000");
    assert_eq!(metric(&v, "runCount"), Some(0));
}
#[test]
fn retained_breakdown_totals_keep_raw_groups_while_series_normalize_their_labels() {
    let h = Harness::new();
    let s = seed(&h);
    for (name, tool) in [("raw-a", "tool"), ("raw-b", " tool ")] {
        origin(
            &s,
            name,
            "ToolInvocation",
            json!({"version":1,"capabilityId":tool,"transportOrigin":"Mcp","actorId":"human","outcome":"Completed","durationMs":1}),
            T0,
            None,
            None,
        );
    }
    let v = query(&s, &input("McpUsage"));
    let items = v["report"]["items"].as_array().unwrap();
    let groups = items
        .iter()
        .filter(|r| r["kind"] == "Breakdown" && r["dimension"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .all(|r| r["label"] == "tool" && r["value"] == "1")
    );
    let series = items
        .iter()
        .filter(|r| {
            r["kind"] == "SeriesPoint" && r["key"].as_str().unwrap().starts_with("breakdown:tool:")
        })
        .collect::<Vec<_>>();
    let keys = series
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 1);
    assert!(series.iter().any(|r| r["value"] == "2"));
}
#[test]
fn feature_reports_do_not_fill_missing_historical_ownership_from_a_current_parent() {
    let h = Harness::new();
    let s = seed(&h);
    origin(
        &s,
        "old-unowned",
        "SessionUsage",
        json!({"version":1,"item":{"purpose":"Conversation","modelId":"sol","inputTokens":"9","outputTokens":"1","cacheReadTokens":"0","cacheWriteTokens":"0"}}),
        T0,
        Some(100),
        Some(id("old-request")),
    );
    let mut i = input("Attempts");
    i.root_id = Some(devworks_core::protocol::RootId(id("feature")));
    let v = query(&s, &i);
    assert!(v["page"]["items"].as_array().unwrap().is_empty());
    assert_eq!(v["page"]["totals"]["legacyObservationMicroUsd"], "0");
    assert!(gap(&v, "historical-feature-ownership-unrecorded"));
}
#[test]
fn retained_signed_endpoints_accept_real_pre_epoch_dates_and_negative_end_defaults() {
    let h = Harness::new();
    let s = seed(&h);
    let mut i = input("AssistantActivity");
    i.start_ms = Some(devworks_core::protocol::support::SignedCounter(
        "-315619200000".into(),
    )); // 1960-01-01 UTC
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        "-283996800000".into(),
    )); // 1961-01-01 UTC
    let v = query(&s, &i);
    assert_eq!(v["report"]["context"]["range"]["startMs"], "-315619200000");
    assert_eq!(v["report"]["context"]["range"]["endMs"], "-283996800000");
    assert_eq!(v["report"]["context"]["interval"], "month");
    assert_eq!(metric(&v, "runCount"), Some(0));
    i.start_ms = None;
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        "-315619200000".into(),
    ));
    let v = query(&s, &i);
    assert_eq!(v["report"]["context"]["range"]["startMs"], "-318211200000");
    assert_eq!(v["report"]["context"]["range"]["endMs"], "-315619200000");
    i.start_ms = Some(devworks_core::protocol::support::SignedCounter(
        "-283996800000".into(),
    ));
    assert!(
        read(&s, |r| Ok(reports::query(r, &ctx(&s), &i, T0).is_err())),
        "reversed ranges retain the source error"
    );
    i.start_ms = None;
    i.end_ms = Some(devworks_core::protocol::support::SignedCounter(
        "-62135596800000".into(),
    ));
    assert!(
        read(&s, |r| Ok(reports::query(r, &ctx(&s), &i, T0).is_err())),
        "DateTime year1 cannot subtract thirty days, just like frozen AddDays"
    );
}
#[test]
fn an_unobserved_actual_model_cannot_establish_zero_usage_for_a_served_model_filter() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "unobserved", "Assistant", T0);
    attempt(
        &h,
        &s,
        "unobserved-attempt",
        "unobserved",
        1,
        (Some(4), Some(2), 3),
    );
    // Read-only fixture: the actual observed identity is absent; intended upstream remains
    // separately captured in pricing_snapshot and cannot be promoted to observed identity.
    h.raw().execute("UPDATE usage_attempts SET provider_model_id=NULL WHERE attempt_id='unobserved-attempt'",[]).unwrap();
    let mut i = input("AssistantUsage");
    i.model = Some(devworks_core::protocol::support::BoundedText(
        "gpt-6.1-sol".into(),
    ));
    let v = query(&s, &i);
    assert_eq!(metric(&v, "totalTokens"), None);
    assert!(gap(&v, "filter-identity-unrecorded"));
    i.model = Some(devworks_core::protocol::support::BoundedText("sol".into()));
    let v = query(&s, &i);
    assert_eq!(metric(&v, "totalTokens"), Some(6));
    assert!(gap(&v, "actual-served-model-unrecorded"));
    let v = query(&s, &input("Attempts"));
    assert!(v["page"]["items"][0].get("resolvedModelId").is_none());
}
#[test]
fn full_dimensional_unknown_cost_reasons_are_current_and_never_truncated_to_a_quote_reason() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "unit-cost", "Assistant", T0);
    attempt(
        &h,
        &s,
        "unit-cost-attempt",
        "unit-cost",
        1,
        (Some(4), Some(2), 3),
    );
    let expected = run_write(&s, WriteOptions::default(), |tx| {
        let a = tx
            .usage()
            .inference_attempt(&id("unit-cost-attempt"))?
            .unwrap();
        let admitted = devworks_core::services::usage::accounting::snapshot(&a)?;
        let mut b = tx.usage().attempt_balance(&a.attempt_id)?.unwrap();
        let mut current: BalanceSummary = serde_json::from_value(b.summary).unwrap();
        current.meter.quantities.additional = (0..128)
            .map(
                |n| devworks_core::services::usage::meter::MeasuredQuantity {
                    modality: "m".repeat(128),
                    charge_kind: "c".repeat(128),
                    unit: format!("{n:03}{}", "u".repeat(125)),
                    cache_write_ttl_ms: None,
                    quantity: 1,
                },
            )
            .collect();
        let estimate = devworks_core::services::usage::pricing::estimate(
            &admitted.quote,
            &current.meter.quantities,
            &admitted.pricing,
        )
        .unwrap();
        current.unknown_components = estimate.unknown_components;
        let reason = current.unknown_components.join(", ");
        assert!(reason.chars().count() > 4096 && reason.chars().count() <= 65536);
        b.revision += 1;
        b.priced_micro_usd = estimate.tracked_micro_usd;
        b.monetary_complete = false;
        b.incomplete = true;
        b.summary = serde_json::to_value(current).unwrap();
        tx.usage().put_attempt_balance(&b, Some(1))?;
        Ok(reason)
    })
    .unwrap();
    let v = query(&s, &input("Attempts"));
    let row = &v["page"]["items"][0];
    assert_eq!(row["cost"]["untrackedReason"], expected);
    assert_eq!(row["unitQuantities"].as_array().unwrap().len(), 128);
    assert_eq!(row["cost"]["monetaryComplete"], false);
    assert!(gap(&v, "monetary-coverage-incomplete"));
}
#[test]
fn aggregate_observed_subtotals_never_claim_complete_coverage_of_an_unknown_remote_tail() {
    let h = Harness::new();
    let s = seed(&h);
    request(&h, "tail", "Assistant", T0);
    attempt(&h, &s, "tail-attempt", "tail", 1, (Some(4), Some(2), 3));
    run_write(&s, WriteOptions::default(), |tx| {
        let mut b = tx.usage().attempt_balance(&id("tail-attempt"))?.unwrap();
        b.revision += 1;
        b.unknown_tail = true;
        b.incomplete = true;
        b.monetary_complete = false;
        tx.usage().put_attempt_balance(&b, Some(1))
    })
    .unwrap();
    let v = query(&s, &input("AssistantUsage"));
    assert_eq!(
        metric(&v, "totalTokens"),
        Some(6),
        "the measured subtotal remains observable"
    );
    assert!(gap(&v, "remote-outcome-unknown"));
    assert!(gap(&v, "usage-observation-incomplete"));
    assert_eq!(v["report"]["coverage"]["state"], "Partial");
}

#[test]
fn full_datetime_ranges_page_synthetic_buckets_without_expanding_them() {
    let h = Harness::new();
    let s = seed(&h);
    for interval in ["day", "week", "month"] {
        let mut raw = json!({"reportKind":"AssistantActivity","startMs":"-62135596800000",
            "endMs":"253402300799999","interval":interval,"page":{"limit":3}});
        let first = query(&s, &serde_json::from_value(raw.clone()).unwrap());
        assert_eq!(first["report"]["items"].as_array().unwrap().len(), 3);
        assert_eq!(first["report"]["hasMore"], true);
        let cursor = first["report"]["nextCursor"].clone();
        raw["page"]["cursor"] = cursor;
        let second = query(&s, &serde_json::from_value(raw.clone()).unwrap());
        assert_eq!(second["report"]["items"].as_array().unwrap().len(), 3);
        assert_eq!(second["report"]["items"][2]["kind"], "SeriesPoint");
        assert_eq!(second["report"]["items"][2]["value"], "0");
        assert_eq!(
            second["report"]["items"][2]["bucketStartMs"],
            "-62135596800000"
        );
        assert_eq!(second, query(&s, &serde_json::from_value(raw).unwrap()));
    }
}
