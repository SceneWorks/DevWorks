//! Sample frames of the Usage screen's requests: reports, filter discovery, the price catalog,
//! tariff overrides, price refresh and the refresh status poll.
use super::{Silent, from};
use crate::frame;
use crate::usage_ops::{self as uops, command, query};
use crate::usage_screen::{Filters, PriceDraft, TariffForm};
use devworks_core::operations::router::PriceStatusInput;
use devworks_core::protocol::OperationId;
use serde_json::json;

fn filters() -> Filters {
    Filters {
        project: Some("project-a".into()),
        start: " 2026-11-05T00:00:00Z ".into(),
        end: "2026-11-06T12:30:00Z".into(),
        interval: "Day".into(),
        user: "user-a".into(),
        provider: "openai".into(),
        model: "gpt-x".into(),
        tool: "tool-a".into(),
        purpose: "review".into(),
        root: "root-a".into(),
        session: "session-a".into(),
        job_run: "run-a".into(),
    }
}

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let default = Filters::default();
    let full = filters();
    let report = |f: &Filters, kind: &str, cursor: Option<&str>| {
        query(b, f.body(kind, cursor).expect("filters"))
    };
    let price = |input: Option<PriceStatusInput>, cursor: Option<&str>| {
        query(b, uops::price_page(input.expect("price filters"), cursor))
    };
    let tariff = TariffForm {
        upstream: "gpt-x".into(),
        amount: " 1.5 ".into(),
        lower: "128000".into(),
        ttl: "300".into(),
        region: "eu".into(),
        profile: "Embedding".into(),
        ..TariffForm::default()
    };
    let draft = PriceDraft {
        model: "model-a".into(),
        revision: "3".into(),
        tariffs: vec![
            tariff.clone(),
            TariffForm {
                upstream: "gpt-y".into(),
                amount: "2".into(),
                ..TariffForm::default()
            },
        ],
    };
    let tariffs = draft
        .tariffs
        .iter()
        .map(|t| t.tariff().expect("tariff"))
        .collect::<Vec<_>>();
    let revision: devworks_core::protocol::Revision = from(json!("7"));
    vec![
        (
            "usage_report_default",
            report(&default, "AssistantUsage", None),
        ),
        ("usage_report_full", report(&full, "McpUsage", None)),
        (
            "usage_attempts_more",
            report(&default, "Attempts", Some("c1")),
        ),
        ("usage_filter_options", report(&full, "Filters", Some("c1"))),
        (
            "usage_price_status",
            price(uops::price_status(&["m1", "m2"], None, None, false), None),
        ),
        (
            "usage_price_status_filtered",
            price(
                uops::price_status(&[], Some(&["openai"]), Some(&["snap-1", "snap-2"]), true),
                None,
            ),
        ),
        (
            "usage_price_status_more",
            price(
                uops::price_status(&[], Some(&["openai"]), None, true),
                Some("c1"),
            ),
        ),
        ("usage_price_probe", query(b, uops::price_probe())),
        (
            "usage_price_override_set",
            command(
                b,
                uops::price_override(&draft.model, &draft.revision, Some(tariffs)),
            ),
        ),
        (
            "usage_price_override_clear",
            command(b, uops::price_override(&draft.model, &draft.revision, None)),
        ),
        (
            "usage_price_refresh",
            command(
                b,
                uops::price_refresh(&["openai".into(), "anthropic".into()], &revision),
            ),
        ),
        (
            "usage_operation_status",
            uops::operation_status(b, &OperationId(frame::id("operation-a").unwrap())),
        ),
    ]
}
