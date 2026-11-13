//! The usage host's store work follows the request, not history: a streamed frame costs no
//! store read, and recovery visits only attempts that still hold local custody.
use super::*;
use devworks_core::providers::decode::DecodedFrame;
use devworks_core::services::usage::{
    accounting::{Admission, AttemptEnd},
    host::{AccountingWriteFence, AttemptHost, OwnedInferenceSink, recovery_candidates},
};

/// A Host attempt request with its own logical request; `n` keeps admissions distinct.
fn host_attempt(
    f: &Fx,
    n: usize,
) -> (
    devworks_core::context::InvocationContext,
    RequestAdmission,
    AttemptRequest,
) {
    let info = f.store.info();
    let context = devworks_core::context::HostIngress::new(f.store.as_ref()).context(
        info.host_actor,
        PrincipalKind::Host,
        id(&format!("bounded-invocation-{n}")),
        id("native-bounded"),
        devworks_core::protocol::Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let logical = RequestAdmission {
        request_id: id(&format!("bounded-request-{n}")),
        purpose: Purpose::Conversation,
        session_id: None,
        project_id: None,
        root_id: None,
        model_id: id("usage-model"),
        connection_id: id("usage-connection"),
        pin_id: None,
        args_digest: "a".repeat(64),
    };
    let request = AttemptRequest {
        admission_key: id(&format!("bounded-admission-{n}")),
        request_id: logical.request_id.clone(),
        actor_id: context.principal().clone(),
        work_id: None,
        job_run_id: None,
        role: "Host".into(),
        query: PriceQuery {
            catalog_model_id: id("usage-model"),
            provider_model_id: Some("native-metadata-source".into()),
            connection_id: id("usage-connection"),
            endpoint_profile: "CompatibleChat".into(),
            access_mode: "host-derived".into(),
            service_tier: None,
            input_tokens: Some(10),
            cache_write_ttl_ms: None,
            processing_region: None,
            required_modalities: vec!["Text".into()],
        },
        quote: PriceQuote {
            classification: CostClassification::LocalFree,
            connection_revision: Some(1),
            publication_provider: None,
            snapshot_id: None,
            price_revision: None,
            override_revision: None,
            currency: "USD".into(),
            rates: None,
            tariffs: vec![],
            untracked_reason: None,
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
            intent: RouteIntent::Model {
                model_id: id("usage-model"),
            },
            effort: EffortName::Default,
            required_capabilities: vec![],
            pin: None,
        },
        input_tokens: 10,
        output_headroom: 10,
        unit_headroom: vec![],
        expected_context_epoch: None,
        dispatch: None,
        cache_diagnostics: vec![],
    };
    (context, logical, request)
}

/// Streams `frames` text deltas, then completes.
struct StreamingSource {
    frames: usize,
}
impl OwnedInferenceSource for StreamingSource {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        Ok(())
    }
    fn next(&mut self, _: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        if self.frames == 0 {
            return Ok(Some(InferenceFrame::Terminal(AttemptEnd::Completed)));
        }
        self.frames -= 1;
        Ok(Some(InferenceFrame::Output(DecodedFrame::TextDelta {
            block_index: 0,
            content_index: 0,
            text: "x".into(),
        })))
    }
    fn close(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}
struct DiscardingSink;
impl OwnedInferenceSink for DiscardingSink {
    fn accept(
        &mut self,
        _: &StoredInferenceAttempt,
        _: DecodedFrame,
        _: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        Ok(())
    }
    fn finish(
        &mut self,
        _: &StoredInferenceAttempt,
        _: AttemptEnd,
        _: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        Ok(())
    }
}

/// AC2 and the recovery scan: a drive opens the same read snapshots and does the same read
/// work for 3 frames as for 30, and a recovery page costs the same behind 10 or 100 settled
/// attempts.
#[test]
fn a_streamed_frame_and_settled_history_cost_the_usage_host_no_store_work() {
    let _native_case = native_runtime_case();
    let h = Harness::new();
    let counted = h.open_counted();
    seed(&h);
    let f = Fx::over(h, counted.store.clone());
    let host = AttemptHost::new(f.store.clone(), f.service.clone(), router(&f));
    let mut n = 0;
    let mut admit = || {
        n += 1;
        let (context, logical, request) = host_attempt(&f, n);
        let Admission::Admitted { attempt, .. } = host
            .admit(context, logical, ReportCategory::Other, request)
            .unwrap()
        else {
            panic!("not admitted")
        };
        attempt.attempt_id
    };
    let drive = |attempt: Id, frames: usize| {
        counted
            .count(|| {
                let outcome = host
                    .drive_with_sink(
                        attempt,
                        Box::new(StreamingSource { frames }),
                        Some(Box::new(DiscardingSink)),
                        Cancellation::new(),
                    )
                    .unwrap();
                assert!(matches!(outcome, DriveOutcome::Settled(_)), "{outcome:?}");
            })
            .1
    };
    let few = drive(admit(), 3);
    let many = drive(admit(), 30);
    eprintln!("usage drive: 3 frames {few:?}, 30 frames {many:?}");
    assert_eq!(
        (few.snapshots, few.read_steps),
        (many.snapshots, many.read_steps),
        "a streamed frame must cost no store read"
    );

    // One attempt still holds custody; settled history grows from 10 to 100 attempts.
    let _open = admit();
    let boot = f.store.info().boot_id.unwrap();
    let recovery = || {
        counted
            .count(|| {
                read(f.store.as_ref(), |s| {
                    recovery_candidates(s, boot.clone(), None, 64).map(|_| ())
                })
            })
            .1
    };
    for _ in 0..10 {
        let attempt = admit();
        drive(attempt, 0);
    }
    let small = recovery();
    for _ in 0..90 {
        let attempt = admit();
        drive(attempt, 0);
    }
    let large = recovery();
    assert_no_growth("usage recovery page", small, large);
}
