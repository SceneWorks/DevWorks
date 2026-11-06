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
        let fx = Fx::new();
        fx.dir.set(
            "OpenAiResponses",
            "api",
            ConnectionState::Connected,
            Some(&["upstream/accounted"]),
        );
        let mut model = custom("accounted", "OpenAiResponses", false, Some(128_000));
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
                endpoint_profile: "OpenAiResponses".into(),
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
    assert!(
        f.host()
            .drive(attempt.attempt_id, src, Cancellation::new())
            .is_err()
    );
    assert!(events.lock().unwrap().is_empty());
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
            assert!(!b.unknown_tail);
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
fn unknown_tail_recovery_is_bounded_and_preserves_late_billing() {
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
    let c = control.clone();
    let visited =
        fx.write(move |tx| devworks_core::services::usage::host::recover_page(tx, &c, None, 1));
    assert_eq!(
        visited.as_slice(),
        std::slice::from_ref(&attempt.attempt_id)
    );
    let balance = fx
        .read(|s| s.usage().attempt_balance(&attempt.attempt_id))
        .unwrap();
    assert!(balance.local_quiescent && balance.unknown_tail);
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
    assert!(late.local_quiescent && late.unknown_tail);
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
