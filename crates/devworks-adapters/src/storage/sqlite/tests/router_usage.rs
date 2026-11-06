//! Actual native router admission and owned metadata-source driver; no provider/inference.
use super::*;
use devworks_core::ports::usage::{CostClassification, PriceQuery, PriceQuote};
use devworks_core::services::router::accounting::{AttemptSelection, PinFence};
use devworks_core::services::runs::ControlService;
use devworks_core::services::usage::{
    accounting::{self, Admission, AttemptEnd, AttemptRequest, Observation, ReportedCost},
    host::{AttemptHost, DriveOutcome, InferenceFrame, OwnedInferenceSource},
    meter::{InputSemantics, ObservationSemantics, Quantities},
    pricing::PricingContext,
};
use std::collections::VecDeque;

struct NativeFx {
    fx: Fx,
    control: Arc<ControlService>,
    request: AttemptRequest,
}
impl NativeFx {
    fn new() -> Self {
        Self::profile("OpenAiResponses")
    }
    fn profile(profile: &str) -> Self {
        let fx = Fx::new();
        fx.dir.set(
            profile,
            "api",
            ConnectionState::Connected,
            Some(&["upstream/accounted"]),
        );
        let mut model = custom("accounted", profile, false, Some(128_000));
        model.connection_id = Some(id("api"));
        fx.add_model(model);
        fx.write(|tx| {
            let mut c = tx.router().connection(&id("api"))?.unwrap();
            c.lifecycle = "Connected".into();
            c.revision += 1;
            tx.router().put_connection(&c, Some(1))
        });
        let (choice, _) = chosen(fx.route("s1", named("accounted"), "Default", &[]));
        let pin = fx.pin_row("s1").unwrap();
        let control = Arc::new(ControlService::new(fx.h.ids.clone()));
        let request = AttemptRequest {
            admission_key: id("admission"),
            request_id: id("logical"),
            actor_id: actor("user-a"),
            work_id: None,
            job_run_id: None,
            role: "Human".into(),
            query: PriceQuery {
                catalog_model_id: choice.model_id,
                provider_model_id: pin.upstream_model_id,
                connection_id: id("api"),
                endpoint_profile: profile.into(),
                access_mode: "NotBillingAuthority".into(),
                service_tier: None,
                input_tokens: Some(100),
                cache_write_ttl_ms: None,
                processing_region: None,
                required_modalities: vec!["Text".into()],
            },
            quote: PriceQuote {
                classification: CostClassification::Untracked,
                connection_revision: Some(2),
                publication_provider: None,
                snapshot_id: None,
                price_revision: None,
                override_revision: None,
                currency: "USD".into(),
                rates: None,
                tariffs: vec![],
                untracked_reason: Some("no admitted price".into()),
            },
            price_generation: 1,
            override_revision: 1,
            pricing: PricingContext {
                input_semantics: InputSemantics::Inclusive,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
            selection: AttemptSelection::Conversation {
                intent: choice.intent,
                effort: EffortName::Default,
                required_capabilities: vec![],
                pin: Some(PinFence {
                    id: choice.pin_id,
                    revision: choice.pin_revision.get(),
                }),
            },
            input_tokens: 100,
            output_headroom: 50,
            unit_headroom: vec![],
            expected_context_epoch: Some(0),
            dispatch: None,
            cache_diagnostics: vec![],
        };
        let router = fx.router.clone();
        let ctx = fx.user();
        fx.write(move |tx| {
            router.admit_owned_request(
                tx,
                &ctx,
                &RequestAdmission {
                    request_id: id("logical"),
                    purpose: Purpose::Conversation,
                    session_id: Some(id("s1")),
                    project_id: None,
                    root_id: None,
                    model_id: id("accounted"),
                    connection_id: id("api"),
                    pin_id: Some(pin.pin_id),
                    args_digest: "a".repeat(64),
                },
                devworks_core::services::router::accounting::ReportCategory::Assistant,
            )
        });
        Self {
            fx,
            control,
            request,
        }
    }
    fn admit(&self) -> Admission {
        let r = self.fx.router.clone();
        let c = self.control.clone();
        let ctx = self.fx.user();
        let request = self.request.clone();
        self.fx
            .write(move |tx| r.admit_attempt(tx, &c, &ctx, &request))
    }
    fn admitted(&self) -> StoredInferenceAttempt {
        match self.admit() {
            Admission::Admitted { attempt, .. } => attempt,
            x => panic!("{x:?}"),
        }
    }
    fn host(&self) -> AttemptHost {
        AttemptHost::new(
            self.fx.store.clone(),
            self.control.clone(),
            self.fx.router.clone(),
        )
    }
}
struct Source {
    frames: VecDeque<InferenceFrame>,
    events: Arc<Mutex<Vec<&'static str>>>,
    fail_close: bool,
}
impl OwnedInferenceSource for Source {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        self.events.lock().unwrap().push("start");
        Ok(())
    }
    fn next(&mut self, _: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        Ok(self.frames.pop_front())
    }
    fn close(&mut self) -> Result<(), StorageError> {
        self.events.lock().unwrap().push("close");
        if self.fail_close {
            Err(StorageError::Busy)
        } else {
            Ok(())
        }
    }
}
fn observation(
    key: &str,
    sequence: i64,
    input: Option<i64>,
    reported: Option<&str>,
) -> Observation {
    Observation {
        key: key.into(),
        sequence,
        semantics: ObservationSemantics::Cumulative,
        quantities: Quantities {
            input_tokens: input,
            output_tokens: Some(0),
            ..Default::default()
        },
        reported_cost: reported.map(|amount| ReportedCost {
            amount: amount.into(),
            currency: "USD".into(),
        }),
        provider_model_id: None,
    }
}
fn source(
    frames: Vec<InferenceFrame>,
    fail_close: bool,
) -> (Box<dyn OwnedInferenceSource>, Arc<Mutex<Vec<&'static str>>>) {
    let events = Arc::new(Mutex::new(vec![]));
    (
        Box::new(Source {
            frames: frames.into(),
            events: events.clone(),
            fail_close,
        }),
        events,
    )
}
#[test]
fn native_attempt_replay_stream_zero_and_late_reported_correction() {
    let f = NativeFx::new();
    let attempt = f.admitted();
    assert!(matches!(
        f.admit(),
        Admission::Admitted { reused: true, .. }
    ));
    let (src, events) = source(
        vec![
            InferenceFrame::Usage(observation("first", 1, Some(100), Some("0.000100"))),
            InferenceFrame::Usage(observation("zero", 2, Some(110), Some("0"))),
            InferenceFrame::Terminal(AttemptEnd::Completed),
        ],
        false,
    );
    let result = f
        .host()
        .drive(attempt.attempt_id.clone(), src, Cancellation::new())
        .unwrap();
    let DriveOutcome::Settled(balance) = result else {
        panic!("{result:?}")
    };
    assert_eq!(&*events.lock().unwrap(), &["start", "close"]);
    assert_eq!(balance.priced_micro_usd, Some(0));
    assert!(balance.local_quiescent && balance.monetary_complete);
    assert_eq!(balance.reserved_micro_usd, None);
    let c = f.control.clone();
    let id = attempt.attempt_id.clone();
    let late = f.fx.write(move |tx| {
        accounting::observe(tx, &c, &id, &observation("late", 3, None, Some("0.000250")))
    });
    assert_eq!(late.priced_micro_usd, Some(250));
    assert!(late.local_quiescent);
    let merged = accounting::summary(&late).unwrap();
    assert_eq!(merged.meter.quantities.input_tokens, Some(110));
    let (src, events) = source(vec![], false);
    let replayed = f
        .host()
        .drive(attempt.attempt_id.clone(), src, Cancellation::new())
        .unwrap();
    assert!(matches!(replayed, DriveOutcome::AlreadyDispatched));
    let Admission::Admitted {
        attempt: settled,
        reused: true,
    } = f.admit()
    else {
        panic!("settled admission must retain its existing source receipt")
    };
    assert_eq!(settled.state, "Settled");
    assert_eq!(settled.attempt_id, attempt.attempt_id);
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(
        f.fx.read(|s| s.usage().attempt_balance(&attempt.attempt_id))
            .unwrap(),
        late
    );
}
#[test]
fn eof_unknown_tail_closes_local_transport_but_failed_close_keeps_guard() {
    for failed in [false, true] {
        let f = NativeFx::new();
        let attempt = f.admitted();
        let (src, events) = source(vec![], failed);
        let result = f
            .host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new());
        assert_eq!(&*events.lock().unwrap(), &["start", "close"]);
        let b =
            f.fx.read(|s| s.usage().attempt_balance(&attempt.attempt_id))
                .unwrap();
        if failed {
            assert!(result.is_err());
            assert!(!b.local_quiescent);
            assert!(b.unknown_tail);
            assert!(
                devworks_core::services::usage::accounting::summary(&b)
                    .unwrap()
                    .custody_recovery_reason
                    .is_some()
            );
        } else {
            assert!(result.is_ok());
            assert!(b.local_quiescent && b.unknown_tail && b.incomplete);
            assert!(!b.monetary_complete);
        }
    }
}
#[test]
fn changed_connection_fences_dispatch_without_opening_source() {
    let f = NativeFx::new();
    let attempt = f.admitted();
    f.fx.write(|tx| {
        let mut c = tx.router().connection(&id("api"))?.unwrap();
        let old = c.revision;
        c.revision += 1;
        c.local_free = true;
        tx.router().put_connection(&c, Some(old))
    });
    let (src, events) = source(vec![], false);
    assert!(matches!(
        f.host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new())
            .unwrap(),
        DriveOutcome::Refused(_)
    ));
    assert!(events.lock().unwrap().is_empty());
    let balance =
        f.fx.read(|s| s.usage().attempt_balance(&attempt.attempt_id))
            .unwrap();
    assert!(balance.local_quiescent);
    assert_eq!(balance.priced_micro_usd, Some(0));
}

#[test]
fn unknown_owner_recovery_retains_custody_and_preserves_late_billing() {
    let f = NativeFx::new();
    let attempt = f.admitted();
    let (src, _) = source(vec![], true);
    assert!(
        f.host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new())
            .is_err()
    );
    let NativeFx { fx, control, .. } = f;
    let fx = fx.restart();
    let boot = fx.store.info().boot_id.unwrap();
    let candidates =
        fx.read(|s| devworks_core::services::usage::host::recovery_candidates(s, boot, None, 1));
    let prepared = devworks_core::services::usage::host::probe_recovery(
        candidates,
        Some(&crate::process::NativeProcessHost),
    )
    .unwrap();
    let c = control.clone();
    let progress =
        fx.write(move |tx| devworks_core::services::usage::host::apply_recovery(tx, &c, prepared));
    assert_eq!(
        progress.visited.as_slice(),
        std::slice::from_ref(&attempt.attempt_id)
    );
    assert_eq!(progress.unresolved, 1);
    let balance = fx
        .read(|s| s.usage().attempt_balance(&attempt.attempt_id))
        .unwrap();
    assert!(!balance.local_quiescent && balance.unknown_tail);
    assert_eq!(balance.reserved_micro_usd, None);
    let c = control.clone();
    let id = attempt.attempt_id.clone();
    let late = fx.write(move |tx| {
        accounting::observe(
            tx,
            &c,
            &id,
            &observation("late-after-restart", 1, Some(100), Some("0.000125")),
        )
    });
    assert_eq!(late.priced_micro_usd, Some(125));
    assert!(!late.local_quiescent && late.unknown_tail);
}
#[test]
fn bootstrap_pages_survive_restart_without_counting_logical_requests_as_money() {
    let f = NativeFx::new();
    for n in 0..7 {
        let router = f.fx.router.clone();
        let ctx = f.fx.user();
        f.fx.write(move |tx| {
            router.admit_owned_request(
                tx,
                &ctx,
                &RequestAdmission {
                    request_id: id(&format!("zero-attempt-{n}")),
                    purpose: Purpose::Title,
                    session_id: None,
                    project_id: None,
                    root_id: None,
                    model_id: id("accounted"),
                    connection_id: id("api"),
                    pin_id: None,
                    args_digest: "b".repeat(64),
                },
                devworks_core::services::router::accounting::ReportCategory::Assistant,
            )
        });
    }
    for _ in 0..4 {
        let page =
            f.fx.write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 1));
        assert!(page.visited <= 1);
        assert!(!page.complete);
    }
    let NativeFx { fx, .. } = f;
    let fx = fx.restart();
    let checkpoint = fx.read(|s| s.usage().usage_bootstrap());
    assert_eq!(checkpoint.phase, "RouterRequests");
    assert_eq!(checkpoint.processed, 2);
    let mut passes = 0;
    loop {
        passes += 1;
        let page = fx.write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 1));
        assert!(page.visited <= 1);
        if page.complete {
            break;
        }
        assert!(passes < 20);
    }
    let origins = fx.read(|s| {
        s.usage()
            .usage_origins(&UsageOriginFilter::default(), None, 500)
    });
    assert_eq!(origins.len(), 8);
    assert!(
        origins
            .iter()
            .all(|o| o.attempt_id.is_none() && o.source_kind == "RouterRequest")
    );
    let count = fx.read(|s| {
        s.usage()
            .inference_attempts(&UsageAttemptFilter::default(), None, 500)
    });
    assert!(count.is_empty());
}
#[test]
fn actual_feature_charge_warns_latches_and_off_preserves_manual_continue() {
    let mut f = NativeFx::new();
    let feature = f.fx.write(|tx| {
        let feature = StoredWork {
            id: id("budget-feature"),
            project_id: project("project-a"),
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Budget feature".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "budget feature".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        };
        tx.work().insert_work(&feature)?;
        Ok(feature)
    });
    let config = feature.clone();
    f.fx.write(move |tx| {
        devworks_core::services::usage::budget::configure(
            tx,
            &config,
            &actor("user-a"),
            &devworks_core::protocol::records::FeatureBudgetInput::Limit {
                micro_usd: Counter::new(100).unwrap(),
            },
        )
    });
    f.request.request_id = id("feature-logical");
    f.request.work_id = Some(feature.id.clone());
    f.request.selection = AttemptSelection::Conversation {
        intent: RouteIntent::Model {
            model_id: id("accounted"),
        },
        effort: EffortName::Default,
        required_capabilities: vec![],
        pin: None,
    };
    let router = f.fx.router.clone();
    let ctx = f.fx.user();
    f.fx.write(move |tx| {
        router.admit_owned_request(
            tx,
            &ctx,
            &RequestAdmission {
                request_id: id("feature-logical"),
                purpose: Purpose::Conversation,
                session_id: Some(id("work-a")),
                project_id: Some(project("project-a")),
                root_id: Some(devworks_core::protocol::RootId(id("budget-feature"))),
                model_id: id("accounted"),
                connection_id: id("api"),
                pin_id: None,
                args_digest: "c".repeat(64),
            },
            devworks_core::services::router::accounting::ReportCategory::Assistant,
        )
    });
    assert!(matches!(
        f.admit(),
        Admission::Blocked(
            devworks_core::services::usage::budget::BudgetBlock::AccountingUnavailable
        )
    ));
    loop {
        if f.fx
            .write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 2))
            .complete
        {
            break;
        }
    }
    let attempt = f.admitted();
    let (src, _) = source(
        vec![
            InferenceFrame::Usage(observation("75-percent", 1, Some(100), Some("0.000075"))),
            InferenceFrame::Terminal(AttemptEnd::Completed),
        ],
        false,
    );
    f.host()
        .drive(attempt.attempt_id.clone(), src, Cancellation::new())
        .unwrap();
    let observed = feature.clone();
    let view =
        f.fx.read(|s| devworks_core::services::usage::budget::view(s, &observed.id));
    assert!(view.warning);
    assert!(!view.continue_required);
    assert_eq!(view.spent_micro_usd.get(), 75);
    let c = f.control.clone();
    let id = attempt.attempt_id;
    f.fx.write(move |tx| {
        accounting::observe(
            tx,
            &c,
            &id,
            &observation("late-cap", 2, None, Some("0.000100")),
        )
    });
    let config = feature.clone();
    f.fx.write(move |tx| {
        devworks_core::services::usage::budget::configure(
            tx,
            &config,
            &actor("user-a"),
            &devworks_core::protocol::records::FeatureBudgetInput::Off {},
        )
    });
    let view =
        f.fx.read(|s| devworks_core::services::usage::budget::view(s, &feature.id));
    assert!(view.continue_required);
    assert_eq!(view.limit_micro_usd, None);
    assert_eq!(view.spent_micro_usd.get(), 100);
}

#[test]
fn production_gateway_publishes_current_usage_and_refuses_lossy_legacy_projection() {
    let mut f = NativeFx::new();
    let attempt = f.admitted();
    let (src, _) = source(
        vec![
            InferenceFrame::Usage(observation("reported", 1, Some(100), Some("0.000010"))),
            InferenceFrame::Terminal(AttemptEnd::Completed),
        ],
        false,
    );
    f.host()
        .drive(attempt.attempt_id, src, Cancellation::new())
        .unwrap();
    loop {
        if f.fx
            .write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 64))
            .complete
        {
            break;
        }
    }
    f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
        devworks_core::services::usage::UsageService::new(f.fx.h.ids.clone(), f.fx.h.clock.clone()),
    ));
    let body = json!({"sessionId":"s1","page":{"limit":10},"reportKind":"Attempts","endMs":(T0+1).to_string()});
    match result(f.fx.query_at(&f.fx.user(), "router.usage", body, Some(3))) {
        OperationResult::RouterUsage {
            value:
                devworks_core::protocol::records::RouterUsageResult::Current(
                    devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                ),
        } => {
            assert_eq!(page.items.0.len(), 1);
            assert_eq!(page.totals.tracked_micro_usd.get(), 10);
            assert_eq!(page.items.0[0].cache_read_tokens, None);
        }
        other => panic!("{other:?}"),
    }
    let legacy = f.fx.query_at(
        &f.fx.user(),
        "router.usage",
        json!({"sessionId":"s1","page":{"limit":10}}),
        Some(1),
    );
    assert_eq!(query_fault(legacy).code, DomainErrorCode::ViewUnavailable);
}

#[test]
fn served_model_preserves_the_actual_text_contract_including_unicode() {
    for model in [
        "x".repeat(513),
        "界".repeat(4096),
        format!("actual\n{}", "λ".repeat(4089)),
    ] {
        let mut f = NativeFx::new();
        let attempt = f.admitted();
        let mut frame = observation("served-model", 1, Some(100), Some("0.000010"));
        frame.provider_model_id = Some(model.clone());
        let (src, _) = source(
            vec![
                InferenceFrame::Usage(frame),
                InferenceFrame::Terminal(AttemptEnd::Completed),
            ],
            false,
        );
        f.host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new())
            .unwrap();
        assert_eq!(
            f.fx.read(|s| s.usage().inference_attempt(&attempt.attempt_id))
                .unwrap()
                .provider_model_id
                .as_deref(),
            Some(model.as_str())
        );
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        match result(f.fx.query_at(
            &f.fx.user(),
            "router.usage",
            json!({"sessionId":"s1","page":{"limit":10},"endMs":(T0+1).to_string()}),
            Some(3),
        )) {
            OperationResult::RouterUsage {
                value:
                    devworks_core::protocol::records::RouterUsageResult::Current(
                        devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                    ),
            } => assert_eq!(
                page.items.0[0]
                    .resolved_model_id
                    .as_ref()
                    .map(|v| v.0.as_str()),
                Some(model.as_str())
            ),
            other => panic!("{other:?}"),
        }
    }
    let f = NativeFx::new();
    let attempt = f.admitted();
    let router = f.fx.router.clone();
    let c = f.control.clone();
    let id = attempt.attempt_id;
    let dispatch_id = id.clone();
    f.fx.write(move |tx| router.mark_attempt_dispatched(tx, &c, &dispatch_id));
    for model in ["界".repeat(4097), "bad\0identity".into()] {
        let c = f.control.clone();
        let id = id.clone();
        let mut frame = observation("invalid", 1, Some(100), None);
        frame.provider_model_id = Some(model);
        assert!(
            run_write(f.fx.store.as_ref(), WriteOptions::default(), move |tx| {
                accounting::observe(tx, &c, &id, &frame)
            })
            .is_err()
        );
    }
}

#[test]
fn admitted_intended_model_is_not_published_as_provider_confirmed() {
    let mut f = NativeFx::new();
    let attempt = f.admitted();
    assert_eq!(attempt.provider_model_id, None);
    let captured = accounting::snapshot(&attempt).unwrap();
    assert_eq!(
        captured.quoted_provider_model_id.as_deref(),
        Some("upstream/accounted")
    );
    f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
        devworks_core::services::usage::UsageService::new(f.fx.h.ids.clone(), f.fx.h.clock.clone()),
    ));
    match result(f.fx.query_at(
        &f.fx.user(),
        "router.usage",
        json!({"sessionId":"s1","page":{"limit":10},"endMs":(T0+1).to_string()}),
        Some(3),
    )) {
        OperationResult::RouterUsage {
            value:
                devworks_core::protocol::records::RouterUsageResult::Current(
                    devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                ),
        } => {
            let row = &page.items.0[0];
            assert_eq!(row.model_id, Some(id("accounted")));
            assert!(row.resolved_model_id.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn actual_configured_quote_stays_immutable_and_reported_cost_replaces_its_estimate() {
    use devworks_core::ports::usage::{PriceResolver, PublishedTariff};
    let mut f = NativeFx::new();
    let tariffs = |input: &str| -> [PublishedTariff; 2] {
        ["Input", "Output"].map(|kind| PublishedTariff {
            upstream_model_id: "upstream/accounted".into(),
            service_tier: "Standard".into(),
            context_lower_exclusive: None,
            context_upper_inclusive: None,
            modality: "Text".into(),
            charge_kind: kind.into(),
            cache_ttl_seconds: None,
            unit: "Token".into(),
            units: 1_000_000,
            amount_usd: if kind == "Input" {
                input.into()
            } else {
                "2".into()
            },
            region: None,
            endpoint_profile: Some("OpenAiResponses".into()),
        })
    };
    let initial = tariffs("1").to_vec();
    f.fx.write(move |tx| {
        tx.usage().put_price_override(
            &StoredPriceOverride {
                model_id: id("accounted"),
                revision: 2,
                mode: "Set".into(),
                tariffs: initial,
                updated_by: actor("user-a"),
                updated_at_ms: T0,
            },
            1,
        )
    });
    f.request.quote = f.fx.read(|read| {
        devworks_core::services::prices::CatalogPriceResolver.quote(read, &f.request.query)
    });
    f.request.override_revision = 2;
    assert_eq!(
        f.request.quote.classification,
        CostClassification::ConfiguredEstimate
    );
    let attempt = f.admitted();
    f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
        devworks_core::services::usage::UsageService::new(f.fx.h.ids.clone(), f.fx.h.clock.clone()),
    ));
    match result(f.fx.query_at(
        &f.fx.user(),
        "router.usage",
        json!({"sessionId":"s1","page":{"limit":10},"endMs":(T0+1).to_string()}),
        Some(3),
    )) {
        OperationResult::RouterUsage {
            value:
                devworks_core::protocol::records::RouterUsageResult::Current(
                    devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                ),
        } => {
            let row = &page.items.0[0];
            assert_eq!(row.state.0, "Admitted");
            assert!(!row.cost.monetary_complete);
            assert!(row.cost.untracked_reason.is_some());
            assert_eq!(row.cost.amount_micro_usd, None);
        }
        other => panic!("{other:?}"),
    }

    let newer = tariffs("100").to_vec();
    f.fx.write(move |tx| {
        tx.usage().put_price_override(
            &StoredPriceOverride {
                model_id: id("accounted"),
                revision: 3,
                mode: "Set".into(),
                tariffs: newer,
                updated_by: actor("user-a"),
                updated_at_ms: T0,
            },
            2,
        )
    });
    assert!(matches!(
        f.admit(),
        Admission::Admitted { reused: true, .. }
    ));
    let (src, _) = source(
        vec![
            InferenceFrame::Usage(observation("estimated", 1, Some(100), None)),
            InferenceFrame::Terminal(AttemptEnd::Completed),
        ],
        false,
    );
    let DriveOutcome::Settled(balance) = f
        .host()
        .drive(attempt.attempt_id.clone(), src, Cancellation::new())
        .unwrap()
    else {
        panic!("unsettled source")
    };
    assert_eq!(balance.priced_micro_usd, Some(100));
    assert_eq!(balance.classification, "ConfiguredEstimate");
    assert!(balance.monetary_complete);
    let c = f.control.clone();
    let id = attempt.attempt_id.clone();
    let corrected = f.fx.write(move |tx| {
        accounting::observe(
            tx,
            &c,
            &id,
            &observation("reported", 2, None, Some("0.000075")),
        )
    });
    assert_eq!(corrected.priced_micro_usd, Some(75));
    assert_eq!(corrected.classification, "ReportedCharge");
    let retained =
        f.fx.read(|s| s.usage().inference_attempt(&attempt.attempt_id))
            .unwrap();
    assert_eq!(
        accounting::snapshot(&retained)
            .unwrap()
            .quote
            .override_revision,
        Some(2)
    );
    let changes = f.fx.read(|s| s.usage().usage_changes(None, None, 500));
    assert!(
        changes
            .iter()
            .any(|e| e.kind == "InferenceUsageObserved" && e.delta_micro_usd == Some(-25))
    );
}

#[test]
fn legacy_gateway_keeps_full_stored_dates_including_maximum_timestamp() {
    for timestamp in [T0, i64::MAX] {
        let mut f = NativeFx::new();
        f.fx.h.clock.0.store(timestamp, Ordering::SeqCst);
        let attempt = f.admitted();
        let mut frame = observation("fully-known", 1, Some(123), Some("0.000030"));
        frame.quantities.cached_read_tokens = Some(0);
        frame.quantities.cache_write_tokens = Some(0);
        let (src, _) = source(
            vec![
                InferenceFrame::Usage(frame),
                InferenceFrame::Terminal(AttemptEnd::Completed),
            ],
            false,
        );
        f.host()
            .drive(attempt.attempt_id, src, Cancellation::new())
            .unwrap();
        if timestamp == T0 {
            f.fx.h.clock.0.store(T0 + 60 * 86_400_000, Ordering::SeqCst);
        }
        loop {
            if f.fx
                .write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 64))
                .complete
            {
                break;
            }
        }
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        match result(f.fx.query_at(
            &f.fx.user(),
            "router.usage",
            json!({"sessionId":"s1","page":{"limit":10}}),
            Some(1),
        )) {
            OperationResult::RouterUsage {
                value: devworks_core::protocol::records::RouterUsageResult::Legacy(report),
            } => {
                assert_eq!(report.items.0.len(), 1);
                assert_eq!(report.items.0[0].input_tokens.get(), 123);
                assert_eq!(report.total_tracked_usd.0, "0.000030");
            }
            other => panic!("{other:?}"),
        }
        let illegal = f.fx.query_at(
            &f.fx.user(),
            "router.usage",
            json!({"sessionId":"s1","page":{"limit":10},"startMs":"-62135596800001","endMs":"0"}),
            Some(3),
        );
        assert_eq!(query_fault(illegal).code, DomainErrorCode::InvalidArgument);
        if timestamp == T0 {
            match result(f.fx.query_at(
                &f.fx.user(),
                "router.usage",
                json!({"sessionId":"s1","page":{"limit":10},"startMs":"-1","endMs":"0"}),
                Some(3),
            )) {
                OperationResult::RouterUsage {
                    value:
                        devworks_core::protocol::records::RouterUsageResult::Current(
                            devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                        ),
                } => assert!(page.items.0.is_empty()),
                other => panic!("{other:?}"),
            }
        }
    }
}

#[test]
fn ttl_unrestricted_legacy_price_survives_actual_quote_settlement_and_gateway_report() {
    use devworks_core::ports::usage::PriceResolver;
    use devworks_core::services::usage::meter::MeasuredQuantity;
    for dimensional in [false, true] {
        let mut f = NativeFx::new();
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_prices(Arc::new(
            devworks_core::services::prices::PriceService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
                false,
            ),
        ));
        let decision=recorded(f.fx.command(&f.fx.user(),"router.price_override",json!({"modelId":"accounted","expectedRevision":"1","price":{"inputUsdPerMillion":"1","outputUsdPerMillion":"2","cacheWriteUsdPerMillion":"3"}}))).0;
        assert!(
            matches!(decision, DomainDecision::Applied { .. }),
            "{decision:?}"
        );
        f.request.query.cache_write_ttl_ms = Some(300_000);
        f.request.pricing.cache_write_possible = true;
        f.request.pricing.cache_write_ttl_ms = Some(300_000);
        f.request.pricing.input_semantics = InputSemantics::Disjoint;
        f.request.quote = f.fx.read(|r| {
            devworks_core::services::prices::CatalogPriceResolver.quote(r, &f.request.query)
        });
        f.request.override_revision = 2;
        assert_eq!(
            f.request.quote.classification,
            CostClassification::ConfiguredEstimate
        );
        assert_eq!(
            f.request.quote.rates.as_ref().unwrap().cache_writes[0].ttl_ms,
            None
        );
        let headroom = MeasuredQuantity {
            modality: "Text".into(),
            charge_kind: "CacheWrite".into(),
            unit: "Token".into(),
            cache_write_ttl_ms: Some(300_000),
            quantity: 20,
        };
        let reserved = devworks_core::services::usage::pricing::reservation(
            &f.request.quote,
            0,
            0,
            &f.request.pricing,
            std::slice::from_ref(&headroom),
        )
        .unwrap();
        assert_eq!(reserved.tracked_micro_usd, Some(60));
        assert!(reserved.monetary_complete);
        let attempt = f.admitted();
        let mut frame = observation("cache-write", 1, Some(0), None);
        frame.quantities.cached_read_tokens = Some(0);
        frame.quantities.cache_write_tokens = Some(20);
        if dimensional {
            frame.quantities.additional.push(MeasuredQuantity {
                modality: "Text".into(),
                charge_kind: "CacheWrite".into(),
                unit: "Token".into(),
                cache_write_ttl_ms: Some(300_000),
                quantity: 20,
            });
        }
        let (src, _) = source(
            vec![
                InferenceFrame::Usage(frame),
                InferenceFrame::Terminal(AttemptEnd::Completed),
            ],
            false,
        );
        let DriveOutcome::Settled(balance) = f
            .host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new())
            .unwrap()
        else {
            panic!("unsettled")
        };
        assert_eq!(
            balance.priced_micro_usd,
            Some(60),
            "dimensional={dimensional}"
        );
        assert!(balance.monetary_complete);
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        let report = result(f.fx.query_at(
            &f.fx.user(),
            "router.usage",
            json!({"sessionId":"s1","page":{"limit":10},"endMs":(T0+1).to_string()}),
            Some(3),
        ));
        let OperationResult::RouterUsage {
            value:
                devworks_core::protocol::records::RouterUsageResult::Current(
                    devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                ),
        } = report
        else {
            panic!("{report:?}")
        };
        assert_eq!(page.totals.tracked_micro_usd.get(), 60);
        assert_eq!(
            page.items.0[0]
                .cache_write_micro_usd
                .as_ref()
                .unwrap()
                .get(),
            60
        );
        let mut ambiguous = f.request.quote.clone();
        // Older admitted quotes carried only the convenience rates, so exercise that
        // fallback independently of the canonical tariff records in the current quote.
        ambiguous.tariffs.clear();
        let rates = ambiguous.rates.as_mut().unwrap();
        let mut duplicate = rates.cache_writes[0].clone();
        duplicate.ttl_ms = Some(300_000);
        rates.cache_writes.push(duplicate);
        let estimate = devworks_core::services::usage::pricing::estimate(
            &ambiguous,
            &Quantities {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cached_read_tokens: Some(0),
                cache_write_tokens: Some(20),
                ..Default::default()
            },
            &f.request.pricing,
        )
        .unwrap();
        assert!(
            !estimate.monetary_complete,
            "unrestricted and exact TTL overlap stays ambiguous"
        );
    }
}

#[test]
fn subscription_reported_usd_never_becomes_api_charge_without_a_matching_tariff() {
    use devworks_core::ports::usage::{PriceResolver, PublishedTariff};
    for profile in ["CodexSubscriptionResponses", "ClaudeSubscriptionMessages"] {
        for priced in [false, true] {
            let mut f = NativeFx::profile(profile);
            if priced {
                let profile = profile.to_owned();
                f.fx.write(move |tx| {
                    tx.usage().put_price_override(
                        &StoredPriceOverride {
                            model_id: id("accounted"),
                            revision: 2,
                            mode: "Set".into(),
                            tariffs: ["Input", "Output"]
                                .map(|kind| PublishedTariff {
                                    upstream_model_id: "upstream/accounted".into(),
                                    service_tier: "Standard".into(),
                                    context_lower_exclusive: None,
                                    context_upper_inclusive: None,
                                    modality: "Text".into(),
                                    charge_kind: kind.into(),
                                    cache_ttl_seconds: None,
                                    unit: "Token".into(),
                                    units: 1_000_000,
                                    amount_usd: "1".into(),
                                    region: None,
                                    endpoint_profile: Some(profile.clone()),
                                })
                                .to_vec(),
                            updated_by: actor("user-a"),
                            updated_at_ms: T0,
                        },
                        1,
                    )
                });
                f.request.override_revision = 2;
            }
            f.request.quote = f.fx.read(|r| {
                devworks_core::services::prices::CatalogPriceResolver.quote(r, &f.request.query)
            });
            assert_eq!(
                f.request.quote.classification,
                if priced {
                    CostClassification::SubscriptionApiEquivalent
                } else {
                    CostClassification::Untracked
                }
            );
            let attempt = f.admitted();
            assert_eq!(attempt.access_mode, "Subscription");
            assert_eq!(
                accounting::snapshot(&attempt).unwrap().query.access_mode,
                "Subscription"
            );
            let (src, _) = source(
                vec![
                    InferenceFrame::Usage(observation(
                        "reported-subscription",
                        1,
                        Some(100),
                        Some("0.012345"),
                    )),
                    InferenceFrame::Terminal(AttemptEnd::Completed),
                ],
                false,
            );
            let DriveOutcome::Settled(balance) = f
                .host()
                .drive(attempt.attempt_id.clone(), src, Cancellation::new())
                .unwrap()
            else {
                panic!("unsettled")
            };
            assert_eq!(
                balance.priced_micro_usd,
                priced.then_some(100),
                "{profile},priced={priced}"
            );
            assert_ne!(balance.classification, "ReportedCharge");
            assert_eq!(
                accounting::summary(&balance)
                    .unwrap()
                    .reported_cost
                    .unwrap()
                    .amount,
                "0.012345"
            );
            let persisted =
                f.fx.read(|r| {
                    r.usage()
                        .usage_observation(&attempt.attempt_id, "reported-subscription")
                })
                .unwrap();
            assert_eq!(persisted.payload["reported_cost"]["amount"], "0.012345");
            f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
                devworks_core::services::usage::UsageService::new(
                    f.fx.h.ids.clone(),
                    f.fx.h.clock.clone(),
                ),
            ));
            let report = result(f.fx.query_at(
                &f.fx.user(),
                "router.usage",
                json!({"sessionId":"s1","page":{"limit":10},"endMs":(T0+1).to_string()}),
                Some(3),
            ));
            let OperationResult::RouterUsage {
                value:
                    devworks_core::protocol::records::RouterUsageResult::Current(
                        devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                    ),
            } = report
            else {
                panic!("{report:?}")
            };
            assert_eq!(page.totals.reported_micro_usd.get(), 0);
            assert_eq!(
                page.totals.tracked_micro_usd.get(),
                if priced { 100 } else { 0 }
            );
            assert_eq!(
                page.items.0[0]
                    .cost
                    .amount_micro_usd
                    .as_ref()
                    .map(|v| v.get()),
                priced.then_some(100)
            );
            assert_eq!(
                page.items.0[0].cost.original_amount.as_ref().unwrap().0,
                "0.012345"
            );
        }
    }
}

#[test]
fn confirmed_cancelled_activity_uses_last_source_outcome_without_masking_later_retries() {
    fn activity(f: &NativeFx) -> Value {
        let value=result(f.fx.query_at(&f.fx.user(),"router.usage",json!({"sessionId":"s1","reportKind":"AssistantActivity","startMs":(T0-1).to_string(),"endMs":(T0+1).to_string(),"page":{"limit":100}}),Some(3)));
        serde_json::to_value(value).unwrap()
    }
    fn metric(value: &Value, key: &str) -> i64 {
        let rows = value["value"]["report"]["items"].as_array().unwrap();
        rows.iter()
            .find(|r| r["kind"] == "Total" && r["key"] == key)
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }
    for later in [None, Some(AttemptEnd::Failed), Some(AttemptEnd::Completed)] {
        let mut f = NativeFx::new();
        let first = f.admitted();
        let (src, _) = source(vec![InferenceFrame::Terminal(AttemptEnd::Cancelled)], false);
        f.host()
            .drive(first.attempt_id, src, Cancellation::new())
            .unwrap();
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        let report = activity(&f);
        assert_eq!(metric(&report, "cancelledRuns"), 1);
        assert_eq!(metric(&report, "failedRuns"), 0);
        assert!(
            report["value"]["report"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["dimension"] == "status" && r["label"] == "cancelled"),
            "{report}"
        );
        let operation = first.operation_id;
        // Seed a recorded retry intent; this report fixture does not add a new retry policy
        // to terminal operations. Admission/dispatch of the subsequent source remains real.
        f.fx.store
            .with_writer_connection(move |conn| {
                super::super::super::conn::db(conn.execute(
                    "UPDATE operations SET state='RetryPending',revision=revision+1 WHERE id=?1",
                    [operation.0.as_str()],
                ))
                .map(|_| ())
            })
            .unwrap();
        f.request.admission_key = id("retry-admission");
        let second = f.admitted();
        assert_eq!(second.attempt_number, 2);
        if let Some(end) = later {
            let (src, _) = source(vec![InferenceFrame::Terminal(end)], false);
            f.host()
                .drive(second.attempt_id, src, Cancellation::new())
                .unwrap();
        } else {
            let router = f.fx.router.clone();
            let control = f.control.clone();
            f.fx.write(move |tx| router.mark_attempt_dispatched(tx, &control, &second.attempt_id));
        }
        let report = activity(&f);
        assert_eq!(metric(&report, "cancelledRuns"), 0);
        assert_eq!(
            metric(&report, "failedRuns"),
            i64::from(later == Some(AttemptEnd::Failed))
        );
        assert_eq!(
            metric(&report, "completedRuns"),
            i64::from(later == Some(AttemptEnd::Completed))
        );
    }
}

#[test]
fn non_million_override_reserves_feature_budget_and_settles_aggregate_tokens_exactly() {
    use devworks_core::ports::usage::PriceResolver;
    use devworks_core::services::usage::budget::{self, BudgetBlock};
    let mut f = NativeFx::new();
    f.fx.cases = std::mem::take(&mut f.fx.cases).with_prices(Arc::new(
        devworks_core::services::prices::PriceService::new(
            f.fx.h.ids.clone(),
            f.fx.h.clock.clone(),
            false,
        ),
    ));
    let tariffs = [("Input", "0.002"), ("Output", "0.004"), ("CacheRead", "0.0005"), ("CacheWrite", "0.003")]
        .map(|(kind, amount)| json!({"upstreamModelId":"upstream/accounted","serviceTier":"Standard","modality":"Text","chargeKind":kind,"unit":"Token","units":"1000","amountUsd":amount}));
    let decision = recorded(f.fx.command_keyed_at(&f.fx.user(), "router.price_override", "non-million-price", json!({"modelId":"accounted","expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}), 3)).0;
    assert!(
        matches!(decision, DomainDecision::Applied { .. }),
        "{decision:?}"
    );
    f.request.query.cache_write_ttl_ms = Some(300_000);
    f.request.pricing.cache_read_possible = true;
    f.request.pricing.cache_write_possible = true;
    f.request.pricing.cache_write_ttl_ms = Some(300_000);
    f.request.quote = f
        .fx
        .read(|r| devworks_core::services::prices::CatalogPriceResolver.quote(r, &f.request.query));
    f.request.override_revision = 2;
    assert_eq!(f.request.quote.tariffs.len(), 4);
    assert!(
        f.request.quote.rates.is_none(),
        "the lossy per-million projection is unnecessary"
    );
    let feature = f.fx.write(|tx| {
        let feature = StoredWork {
            id: id("denominator-feature"),
            project_id: project("project-a"),
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Exact denominator feature".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "denominator feature".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        };
        tx.work().insert_work(&feature)?;
        budget::configure(
            tx,
            &feature,
            &actor("user-a"),
            &devworks_core::protocol::records::FeatureBudgetInput::Limit {
                micro_usd: Counter::new(499).unwrap(),
            },
        )?;
        Ok(feature)
    });
    f.request.request_id = id("denominator-logical");
    f.request.work_id = Some(feature.id.clone());
    f.request.selection = AttemptSelection::Conversation {
        intent: RouteIntent::Model {
            model_id: id("accounted"),
        },
        effort: EffortName::Default,
        required_capabilities: vec![],
        pin: None,
    };
    let router = f.fx.router.clone();
    let ctx = f.fx.user();
    f.fx.write(move |tx| {
        router.admit_owned_request(
            tx,
            &ctx,
            &RequestAdmission {
                request_id: id("denominator-logical"),
                purpose: Purpose::Conversation,
                session_id: Some(id("work-a")),
                project_id: Some(project("project-a")),
                root_id: Some(devworks_core::protocol::RootId(id("denominator-feature"))),
                model_id: id("accounted"),
                connection_id: id("api"),
                pin_id: None,
                args_digest: "d".repeat(64),
            },
            devworks_core::services::router::accounting::ReportCategory::Assistant,
        )
    });
    while !f
        .fx
        .write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 64))
        .complete
    {}
    assert!(
        matches!(f.admit(), Admission::Blocked(BudgetBlock::ReservationWait)),
        "the exact 500-micro reservation exceeds the 499-micro cap"
    );
    let configured = feature.clone();
    f.fx.write(move |tx| {
        budget::configure(
            tx,
            &configured,
            &actor("user-a"),
            &devworks_core::protocol::records::FeatureBudgetInput::Limit {
                micro_usd: Counter::new(500).unwrap(),
            },
        )
    });
    let attempt = f.admitted();
    let admitted =
        f.fx.read(|r| r.usage().attempt_balance(&attempt.attempt_id))
            .unwrap();
    assert_eq!(admitted.reserved_micro_usd, Some(500));
    let view = f.fx.read(|r| budget::view(r, &feature.id));
    assert_eq!(view.reserved_micro_usd.get(), 500);
    let mut frame = observation("non-million-aggregate", 1, Some(100), None);
    frame.quantities.output_tokens = Some(10);
    frame.quantities.cached_read_tokens = Some(40);
    frame.quantities.cache_write_tokens = Some(20);
    let (src, _) = source(
        vec![
            InferenceFrame::Usage(frame),
            InferenceFrame::Terminal(AttemptEnd::Completed),
        ],
        false,
    );
    let DriveOutcome::Settled(balance) = f
        .host()
        .drive(attempt.attempt_id.clone(), src, Cancellation::new())
        .unwrap()
    else {
        panic!("unsettled")
    };
    assert_eq!(balance.priced_micro_usd, Some(200));
    assert!(balance.monetary_complete);
    assert_eq!(balance.reserved_micro_usd, None);
    let view = f.fx.read(|r| budget::view(r, &feature.id));
    assert_eq!(view.spent_micro_usd.get(), 200);
    assert_eq!(view.reserved_micro_usd.get(), 0);
    f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
        devworks_core::services::usage::UsageService::new(f.fx.h.ids.clone(), f.fx.h.clock.clone()),
    ));
    let report = result(f.fx.query_at(
        &f.fx.user(),
        "router.usage",
        json!({"sessionId":"work-a","page":{"limit":10},"endMs":(T0+1).to_string()}),
        Some(3),
    ));
    let OperationResult::RouterUsage {
        value:
            devworks_core::protocol::records::RouterUsageResult::Current(
                devworks_core::protocol::records::UsageQueryResult::Attempts { page },
            ),
    } = report
    else {
        panic!("{report:?}")
    };
    assert_eq!(page.totals.tracked_micro_usd.get(), 200);
    assert_eq!(
        page.items.0[0].cache_read_micro_usd.as_ref().unwrap().get(),
        20
    );
    assert_eq!(
        page.items.0[0]
            .cache_write_micro_usd
            .as_ref()
            .unwrap()
            .get(),
        60
    );
    assert!(page.items.0[0].cost.monetary_complete);
}

#[test]
fn actual_non_million_quotes_preserve_large_units_zero_and_overlapping_rate_unknowns() {
    use devworks_core::ports::usage::PriceResolver;
    // Independent exact Fraction references: 150 * (999999999999 * 1e6 / i64::MAX)
    // reserves 17 micros; 110 tokens settle to 12. No per-million decimal can replace it.
    for (units, amount, overlap, reserved, incurred, complete) in [
        (i64::MAX, "999999999999", false, 17, 12, true),
        (1000, "0", false, 0, 0, true),
        (1000, "0.002", true, 100, 20, false),
    ] {
        let mut f = NativeFx::new();
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_prices(Arc::new(
            devworks_core::services::prices::PriceService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
                false,
            ),
        ));
        let mut tariffs = ["Input", "Output"].map(|kind|json!({"upstreamModelId":"upstream/accounted","serviceTier":"Standard","modality":"Text","chargeKind":kind,"unit":"Token","units":units.to_string(),"amountUsd":amount})).to_vec();
        if overlap {
            // A valid Any row overlaps Text. The resolver's million-only projection contains
            // zero, but it cannot hide the conflicting canonical exact-denominator input row.
            tariffs.push(json!({"upstreamModelId":"upstream/accounted","serviceTier":"Standard","modality":"Any","chargeKind":"Input","unit":"Token","units":"1000000","amountUsd":"0"}));
        }
        let decision=recorded(f.fx.command_keyed_at(&f.fx.user(),"router.price_override","exact-units-price",json!({"modelId":"accounted","expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}),3)).0;
        assert!(
            matches!(decision, DomainDecision::Applied { .. }),
            "{decision:?}"
        );
        f.request.quote = f.fx.read(|r| {
            devworks_core::services::prices::CatalogPriceResolver.quote(r, &f.request.query)
        });
        f.request.override_revision = 2;
        assert_eq!(
            f.request.quote.classification,
            CostClassification::ConfiguredEstimate
        );
        assert_eq!(f.request.quote.tariffs[0].units, units);
        let attempt = f.admitted();
        let before =
            f.fx.read(|r| r.usage().attempt_balance(&attempt.attempt_id))
                .unwrap();
        assert_eq!(
            before.reserved_micro_usd,
            Some(reserved),
            "units={units},overlap={overlap}"
        );
        let mut frame = observation("exact-units-observation", 1, Some(100), None);
        frame.quantities.output_tokens = Some(10);
        let (src, _) = source(
            vec![
                InferenceFrame::Usage(frame),
                InferenceFrame::Terminal(AttemptEnd::Completed),
            ],
            false,
        );
        let DriveOutcome::Settled(balance) = f
            .host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new())
            .unwrap()
        else {
            panic!("unsettled")
        };
        assert_eq!(
            balance.priced_micro_usd,
            Some(incurred),
            "units={units},overlap={overlap}"
        );
        assert_eq!(balance.monetary_complete, complete);
        assert_eq!(balance.reserved_micro_usd, None);
        let retained =
            f.fx.read(|r| r.usage().inference_attempt(&attempt.attempt_id))
                .unwrap();
        assert_eq!(
            accounting::snapshot(&retained).unwrap().quote,
            f.request.quote
        );
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        let report = result(f.fx.query_at(
            &f.fx.user(),
            "router.usage",
            json!({"sessionId":"s1","page":{"limit":10},"endMs":(T0+1).to_string()}),
            Some(3),
        ));
        let OperationResult::RouterUsage {
            value:
                devworks_core::protocol::records::RouterUsageResult::Current(
                    devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                ),
        } = report
        else {
            panic!("{report:?}")
        };
        assert_eq!(page.totals.tracked_micro_usd.get(), incurred);
        assert_eq!(page.items.0[0].cost.monetary_complete, complete);
        assert_eq!(
            page.items.0[0]
                .cost
                .amount_micro_usd
                .as_ref()
                .unwrap()
                .get(),
            incurred
        );
        assert_eq!(page.items.0[0].cost.untracked_reason.is_some(), !complete);
    }
}

#[test]
fn actual_feature_reservation_uses_only_host_possible_cache_reads_and_keeps_missing_rates_partial()
{
    use devworks_core::ports::usage::PriceResolver;
    use devworks_core::services::usage::budget::{self, BudgetBlock};
    for (read_possible, read_priced, reserved, incurred, complete) in [
        (true, true, 300, 300, true),
        (false, true, 200, 200, true),
        (true, false, 200, 0, false),
    ] {
        let mut f = NativeFx::new();
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_prices(Arc::new(
            devworks_core::services::prices::PriceService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
                false,
            ),
        ));
        let mut tariffs=[("Input","0.002"),("Output","0"),("CacheWrite","0.0015")].map(|(kind,amount)|json!({"upstreamModelId":"upstream/accounted","serviceTier":"Standard","modality":"Text","chargeKind":kind,"unit":"Token","units":"1000","amountUsd":amount})).to_vec();
        if read_priced {
            tariffs.push(json!({"upstreamModelId":"upstream/accounted","serviceTier":"Standard","modality":"Text","chargeKind":"CacheRead","unit":"Token","units":"1000","amountUsd":"0.003"}));
        }
        let decision=recorded(f.fx.command_keyed_at(&f.fx.user(),"router.price_override","read-reservation-price",json!({"modelId":"accounted","expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}),3)).0;
        assert!(
            matches!(decision, DomainDecision::Applied { .. }),
            "{decision:?}"
        );
        f.request.pricing.cache_read_possible = read_possible;
        f.request.pricing.cache_write_possible = true;
        f.request.pricing.cache_write_ttl_ms = Some(300_000);
        f.request.query.cache_write_ttl_ms = Some(300_000);
        f.request.output_headroom = 0;
        f.request.quote = f.fx.read(|r| {
            devworks_core::services::prices::CatalogPriceResolver.quote(r, &f.request.query)
        });
        f.request.override_revision = 2;
        let feature = f.fx.write(|tx| {
            let feature = StoredWork {
                id: id("read-budget-feature"),
                project_id: project("project-a"),
                kind: FEATURE.into(),
                parent_feature_id: None,
                status_id: None,
                title: "Cache read budget".into(),
                priority: 0,
                manual_order: 0,
                revision: 1,
                scope_revision: 1,
                created_at_ms: T0,
                updated_at_ms: T0,
                created_by: None,
                updated_by: None,
                search_key: "cache read budget".into(),
                caller_ref: None,
                completion: None,
                brief: json!({}),
            };
            tx.work().insert_work(&feature)?;
            budget::configure(
                tx,
                &feature,
                &actor("user-a"),
                &devworks_core::protocol::records::FeatureBudgetInput::Limit {
                    micro_usd: Counter::new(250).unwrap(),
                },
            )?;
            Ok(feature)
        });
        f.request.work_id = Some(feature.id.clone());
        f.request.request_id = id("read-budget-logical");
        f.request.selection = AttemptSelection::Conversation {
            intent: RouteIntent::Model {
                model_id: id("accounted"),
            },
            effort: EffortName::Default,
            required_capabilities: vec![],
            pin: None,
        };
        let router = f.fx.router.clone();
        let ctx = f.fx.user();
        f.fx.write(move |tx| {
            router.admit_owned_request(
                tx,
                &ctx,
                &RequestAdmission {
                    request_id: id("read-budget-logical"),
                    purpose: Purpose::Conversation,
                    session_id: Some(id("work-a")),
                    project_id: Some(project("project-a")),
                    root_id: Some(devworks_core::protocol::RootId(id("read-budget-feature"))),
                    model_id: id("accounted"),
                    connection_id: id("api"),
                    pin_id: None,
                    args_digest: "e".repeat(64),
                },
                devworks_core::services::router::accounting::ReportCategory::Assistant,
            )
        });
        while !f
            .fx
            .write(|tx| devworks_core::services::usage::ingest::bootstrap_page(tx, 64))
            .complete
        {}
        if reserved > 250 {
            assert!(
                matches!(f.admit(), Admission::Blocked(BudgetBlock::ReservationWait)),
                "the actual possible cache-read charge must fence a 250-micro cap"
            );
            let edited = feature.clone();
            f.fx.write(move |tx| {
                budget::configure(
                    tx,
                    &edited,
                    &actor("user-a"),
                    &devworks_core::protocol::records::FeatureBudgetInput::Limit {
                        micro_usd: Counter::new(301).unwrap(),
                    },
                )
            });
        }
        let attempt = f.admitted();
        let admitted =
            f.fx.read(|r| r.usage().attempt_balance(&attempt.attempt_id))
                .unwrap();
        assert_eq!(admitted.reserved_micro_usd, Some(reserved));
        assert_eq!(
            accounting::summary(&admitted)
                .unwrap()
                .unknown_components
                .contains(&"cache read reservation".into()),
            read_possible && !read_priced
        );
        let mut frame = observation("read-priced-observation", 1, Some(100), None);
        frame.quantities.cached_read_tokens = Some(if read_possible { 100 } else { 0 });
        frame.quantities.cache_write_tokens = Some(0);
        let (src, _) = source(
            vec![
                InferenceFrame::Usage(frame),
                InferenceFrame::Terminal(AttemptEnd::Completed),
            ],
            false,
        );
        let DriveOutcome::Settled(balance) = f
            .host()
            .drive(attempt.attempt_id.clone(), src, Cancellation::new())
            .unwrap()
        else {
            panic!("unsettled")
        };
        assert_eq!(balance.priced_micro_usd, Some(incurred));
        assert_eq!(balance.monetary_complete, complete);
        assert_eq!(balance.reserved_micro_usd, None);
        let budget = f.fx.read(|r| budget::view(r, &feature.id));
        assert_eq!(budget.spent_micro_usd.get(), incurred);
        assert_eq!(budget.reserved_micro_usd.get(), 0);
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        let report = result(f.fx.query_at(
            &f.fx.user(),
            "router.usage",
            json!({"sessionId":"work-a","page":{"limit":10},"endMs":(T0+1).to_string()}),
            Some(3),
        ));
        let OperationResult::RouterUsage {
            value:
                devworks_core::protocol::records::RouterUsageResult::Current(
                    devworks_core::protocol::records::UsageQueryResult::Attempts { page },
                ),
        } = report
        else {
            panic!("{report:?}")
        };
        assert_eq!(page.totals.tracked_micro_usd.get(), incurred);
        assert_eq!(page.items.0[0].cost.monetary_complete, complete);
        assert_eq!(page.items.0[0].cost.untracked_reason.is_some(), !complete);
    }
}

fn official_quote_fixture() -> NativeFx {
    use devworks_core::ports::usage::PriceResolver;
    let mut f = NativeFx::new();
    f.fx.write(|tx| {
        let mut c = tx.router().connection(&id("api"))?.unwrap();
        let rev = c.revision;
        c.endpoint = "https://api.openai.com/v1".into();
        tx.router().put_connection(&c, Some(rev))?;
        Ok(())
    });
    f.request.quote = f.fx.read(|read| {
        devworks_core::services::prices::CatalogPriceResolver.quote(read, &f.request.query)
    });
    // This field belongs to the opaque producer. It is deliberately not a host publication
    // revision and must remain usable unchanged when a real requote is obtained.
    f.request.price_generation = 7_654_321;
    assert_eq!(
        f.request.quote.classification,
        CostClassification::Untracked
    );
    assert!(f.request.quote.snapshot_id.is_none());
    f
}
fn publish_first_official_price(f: &NativeFx) {
    use devworks_core::ports::usage::PublishedTariff;
    f.fx.write(|tx| {
        let status = StoredPriceStatus {
            provider: "openai".into(),
            revision: 1,
            current_snapshot_id: None,
            last_attempt_ms: Some(T0),
            last_success_ms: None,
            refresh_pending: true,
            last_error: None,
            next_refresh_ms: Some(i64::MAX),
            refresh_operation_id: None,
        };
        tx.usage().put_price_status(&status, None)?;
        tx.usage().publish_price_snapshot(
            &StoredPriceSnapshot {
                id: id("first-official-publication"),
                provider: "openai".into(),
                revision: 1,
                source_url: "https://developers.openai.com/api/docs/pricing".into(),
                source_digest: "a".repeat(64),
                parser_version: "recorded-official".into(),
                fetched_at_ms: T0,
                verified_at_ms: T0,
                effective_date: None,
                tariffs: [("Input", "1"), ("Output", "2")]
                    .map(|(kind, amount)| PublishedTariff {
                        upstream_model_id: "upstream/accounted".into(),
                        service_tier: "Standard".into(),
                        context_lower_exclusive: None,
                        context_upper_inclusive: None,
                        modality: "Text".into(),
                        charge_kind: kind.into(),
                        cache_ttl_seconds: None,
                        unit: "Token".into(),
                        units: 1_000_000,
                        amount_usd: amount.into(),
                        region: None,
                        endpoint_profile: Some("OpenAiResponses".into()),
                    })
                    .to_vec(),
            },
            status.revision,
        )?;
        Ok(())
    });
}
#[test]
fn absent_official_quote_is_fenced_by_first_publication_and_real_requote_reserves() {
    use devworks_core::ports::usage::PriceResolver;
    let mut f = official_quote_fixture();
    let before = f.request.quote.clone();
    publish_first_official_price(&f);
    assert!(
        matches!(f.admit(),Admission::SourceChanged(reason) if reason.contains("publication")),
        "an absent official source is an immutable quotation fact, not permission to bypass the first publication"
    );
    assert!(
        f.fx.read(|r| r
            .usage()
            .inference_attempts(&UsageAttemptFilter::default(), None, 10))
            .is_empty()
    );
    f.request.quote = f.fx.read(|read| {
        devworks_core::services::prices::CatalogPriceResolver.quote(read, &f.request.query)
    });
    assert_eq!(
        f.request.quote.classification,
        CostClassification::OfficialEstimate
    );
    assert_eq!(f.request.price_generation, 7_654_321);
    let attempt = f.admitted();
    let balance =
        f.fx.read(|r| r.usage().attempt_balance(&attempt.attempt_id))
            .unwrap();
    assert_eq!(
        balance.reserved_micro_usd,
        Some(200),
        "100 input at1USD/M plus50 output at2USD/M are reserved from the actual new quote"
    );
    assert!(before.snapshot_id.is_none());
    assert_eq!(before.classification, CostClassification::Untracked);
}
#[test]
fn a_legitimately_admitted_missing_source_quote_remains_immutable_after_publication() {
    let f = official_quote_fixture();
    let attempt = f.admitted();
    let retained = accounting::snapshot(&attempt).unwrap().quote;
    publish_first_official_price(&f);
    assert!(matches!(
        f.admit(),
        Admission::Admitted { reused: true, .. }
    ));
    let (src, _) = source(
        vec![
            InferenceFrame::Usage(observation("actual-token-only", 1, Some(100), None)),
            InferenceFrame::Terminal(AttemptEnd::Completed),
        ],
        false,
    );
    let DriveOutcome::Settled(balance) = f
        .host()
        .drive(attempt.attempt_id.clone(), src, Cancellation::new())
        .unwrap()
    else {
        panic!("unsettled");
    };
    assert_eq!(balance.classification, "Untracked");
    assert!(balance.priced_micro_usd.is_none());
    assert_eq!(
        accounting::snapshot(
            &f.fx
                .read(|r| r.usage().inference_attempt(&attempt.attempt_id))
                .unwrap()
        )
        .unwrap()
        .quote,
        retained
    );
}

#[test]
fn non_official_quote_paths_ignore_unrelated_official_publications() {
    use devworks_core::ports::usage::{PriceResolver, PublishedTariff};
    for mode in ["Custom", "LocalFree", "Configured"] {
        let mut f = NativeFx::new();
        if mode == "LocalFree" {
            f.fx.write(|tx| {
                let mut c = tx.router().connection(&id("api"))?.unwrap();
                let revision = c.revision;
                c.local_free = true;
                tx.router().put_connection(&c, Some(revision))?;
                Ok(())
            });
        }
        if mode == "Configured" {
            f.fx.write(|tx| {
                tx.usage().put_price_override(
                    &StoredPriceOverride {
                        model_id: id("accounted"),
                        revision: 2,
                        mode: "Set".into(),
                        tariffs: [("Input", "1"), ("Output", "2")]
                            .map(|(kind, amount)| PublishedTariff {
                                upstream_model_id: "upstream/accounted".into(),
                                service_tier: "Standard".into(),
                                context_lower_exclusive: None,
                                context_upper_inclusive: None,
                                modality: "Text".into(),
                                charge_kind: kind.into(),
                                cache_ttl_seconds: None,
                                unit: "Token".into(),
                                units: 1_000_000,
                                amount_usd: amount.into(),
                                region: None,
                                endpoint_profile: Some("OpenAiResponses".into()),
                            })
                            .to_vec(),
                        updated_by: actor("user-a"),
                        updated_at_ms: T0,
                    },
                    1,
                )
            });
            f.request.override_revision = 2;
        }
        f.request.quote = f.fx.read(|read| {
            devworks_core::services::prices::CatalogPriceResolver.quote(read, &f.request.query)
        });
        assert!(
            f.request.quote.publication_provider.is_none(),
            "no official source was consulted for{mode}"
        );
        let classification = f.request.quote.classification;
        publish_first_official_price(&f);
        let attempt = f.admitted();
        let retained = accounting::snapshot(&attempt).unwrap();
        assert_eq!(retained.quote.classification, classification);
        let reserved =
            f.fx.read(|r| r.usage().attempt_balance(&attempt.attempt_id))
                .unwrap()
                .reserved_micro_usd;
        assert_eq!(
            reserved,
            match mode {
                "LocalFree" => Some(0),
                "Configured" => Some(200),
                _ => None,
            }
        );
    }
}

#[test]
fn accepted_raw_reported_costs_remain_readable_without_fabricating_paid_spend() {
    for amount in ["0.0000000000001".to_owned(), "9".repeat(128)] {
        let mut f = NativeFx::new();
        let attempt = f.admitted();
        let (src, _) = source(
            vec![
                InferenceFrame::Usage(observation("raw-amount", 1, Some(1), Some(&amount))),
                InferenceFrame::Terminal(AttemptEnd::Completed),
            ],
            false,
        );
        let outcome = f
            .host()
            .drive(attempt.attempt_id, src, Cancellation::new())
            .unwrap();
        let DriveOutcome::Settled(balance) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(balance.priced_micro_usd, None);
        assert!(!balance.monetary_complete);
        assert_eq!(
            accounting::summary(&balance)
                .unwrap()
                .reported_cost
                .unwrap()
                .amount,
            amount
        );
        f.fx.cases = std::mem::take(&mut f.fx.cases).with_usage(Arc::new(
            devworks_core::services::usage::UsageService::new(
                f.fx.h.ids.clone(),
                f.fx.h.clock.clone(),
            ),
        ));
        for kind in [
            "Attempts",
            "AssistantUsage",
            "AssistantActivity",
            "McpUsage",
            "CodeReviewUsage",
            "RepositoryAnalysisUsage",
            "Filters",
        ] {
            let value = serde_json::to_value(result(f.fx.query_at(
                &f.fx.user(),
                "router.usage",
                json!({
                    "sessionId":"s1","reportKind":kind,"startMs":(T0-1).to_string(),
                    "endMs":(T0+1).to_string(),"page":{"limit":100}
                }),
                Some(3),
            )))
            .unwrap();
            if kind == "Attempts" {
                let page = &value["value"]["page"];
                let cost = &page["items"][0]["cost"];
                assert_eq!(cost["originalAmount"], amount);
                assert_eq!(cost["monetaryComplete"], false);
                assert!(cost.get("amountMicroUsd").is_none());
                assert_eq!(page["totals"]["reportedMicroUsd"], "0");
                assert_eq!(page["totals"]["trackedMicroUsd"], "0");
            }
        }
    }
}
