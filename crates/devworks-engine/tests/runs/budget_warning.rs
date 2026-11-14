//! ROUTE-11: a priced charge that takes a feature past 75% of its limit reaches the feature
//! coordinator's retained inbox with the remaining allowance, through the real usage runtime.
use super::*;
use devworks_core::protocol::records::FeatureBudgetInput;
use devworks_core::services::agents::retained::Recipient;
use devworks_core::services::usage::budget;
use devworks_engine::agent_inbox::{InboxSelection, RetainedInboxDriver};

/// One cumulative text observation (10 input, 4 output tokens), then completion.
struct Priced {
    first: bool,
}
impl OwnedInferenceSource for Priced {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        Ok(())
    }
    fn next(&mut self, _: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        if !std::mem::replace(&mut self.first, false) {
            return Ok(Some(InferenceFrame::Terminal(
                devworks_core::services::usage::accounting::AttemptEnd::Completed,
            )));
        }
        Ok(Some(InferenceFrame::Usage(Observation {
            key: "budget-warning-usage".into(),
            sequence: 1,
            semantics: ObservationSemantics::Cumulative,
            quantities: Quantities {
                input_tokens: Some(10),
                output_tokens: Some(4),
                cached_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                ..Default::default()
            },
            reported_cost: None,
            provider_model_id: Some("native-metadata-source".into()),
        })))
    }
    fn close(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}

/// The coordinator's agent session on the feature root, with its primary assignment.
fn coordinator(f: &Fx) -> Id {
    let started = f.command(
        &f.agent,
        "sessions.start",
        json!({"mode":"Command","protocol":PROTOCOL,"requestKey":f.key(),"body":{
            "kind":"sessions.start","scope":{"kind":"Project","projectId":"project-a"},
            "rootId":"root-f","title":"Feature coordinator"}}),
    );
    let value = serde_json::to_value(started).unwrap();
    let result = &value["receipt"]["decision"]["result"]["value"];
    let session = id(result["sessionId"]
        .as_str()
        .or(result["id"].as_str())
        .unwrap());
    let registered = session.clone();
    f.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        svc.register_assignment(
            tx,
            &host,
            NewRunAssignment {
                root: RootId(id("root-f")),
                work: RootId(id("root-f")),
                parent: None,
                session: registered,
                scope_key: None,
            },
        )
        .map(|assigned| assigned.unwrap())
    });
    session
}

#[test]
fn a_charge_past_75_percent_of_the_feature_limit_reaches_the_coordinator_inbox() {
    let _native_case = native_runtime_case();
    runtime().block_on(within("feature budget warning", async {
        let f = Fx::new();
        let router = router(&f);
        applied(&f.start(&f.user, "root-f", 1, 1));
        let _activation = f.launch("native-provider-owner");
        let session = coordinator(&f);
        // 24 micro-USD: the 18 micro-USD charge below is exactly 75% of it.
        f.tx(|_, tx| {
            let work = tx.work().work(&id("root-f"))?.unwrap();
            budget::configure(
                tx,
                &work,
                &ActorId(id("user-a")),
                &FeatureBudgetInput::Limit {
                    micro_usd: devworks_core::protocol::Counter::new(24).unwrap(),
                },
            )
            .map(|_| ())
        });
        // Configured text prices: 10 input tokens at $1/M and 4 output tokens at $2/M.
        write(f.store.as_ref(), |tx| {
            let mut connection = tx.router().connection(&id("usage-connection"))?.unwrap();
            connection.local_free = false;
            let expected = connection.revision;
            tx.router().put_connection(&connection, Some(expected))?;
            let tariff = |kind: &str, amount: &str| devworks_core::ports::usage::PublishedTariff {
                upstream_model_id: "native-metadata-source".into(),
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
                endpoint_profile: None,
            };
            let author = tx.store().host_actor.clone();
            tx.usage()
                .put_price_override(
                    &StoredPriceOverride {
                        model_id: id("usage-model"),
                        revision: 2,
                        mode: "Set".into(),
                        tariffs: vec![tariff("Input", "1"), tariff("Output", "2")],
                        updated_by: author,
                        updated_at_ms: T0,
                    },
                    1,
                )
                .map(|_| ())
        })
        .unwrap();
        // A person's manual feature chat: feature-owned spending with no managed dispatch.
        let context = f.user.clone().with_lineage(
            Some(ProjectId(id("project-a"))),
            Some(RootId(id("root-f"))),
            None,
        );
        let logical = RequestAdmission {
            request_id: id("budget-warning-request"),
            purpose: Purpose::Conversation,
            session_id: None,
            project_id: Some(ProjectId(id("project-a"))),
            root_id: None,
            model_id: id("usage-model"),
            connection_id: id("usage-connection"),
            pin_id: None,
            args_digest: "a".repeat(64),
        };
        let mut request = AttemptRequest {
            admission_key: id("budget-warning-admission"),
            request_id: logical.request_id.clone(),
            actor_id: context.principal().clone(),
            work_id: Some(id("root-f")),
            job_run_id: None,
            role: "Human".into(),
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
            override_revision: 2,
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
            output_headroom: 4,
            unit_headroom: vec![],
            expected_context_epoch: None,
            dispatch: None,
            cache_diagnostics: vec![],
        };
        use devworks_core::ports::usage::PriceResolver;
        request.quote = read(f.store.as_ref(), |s| {
            devworks_core::services::prices::CatalogPriceResolver.quote(s, &request.query)
        });
        request.price_generation = read(f.store.as_ref(), |s| s.usage().price_revision());
        let usage = UsageRuntime::new(f.store.clone(), f.service.clone(), Some(router));
        // Budget admission needs completed usage accounting; the runtime's own tick finishes it.
        while read(f.store.as_ref(), |s| s.usage().usage_bootstrap()).phase != "Complete" {
            usage.tick().await.unwrap();
        }
        let outcome = usage
            .produce(
                context,
                logical,
                ReportCategory::Assistant,
                request,
                Box::new(Priced { first: true }),
            )
            .await
            .unwrap();
        let DriveOutcome::Settled(balance) = outcome else {
            panic!("charge not settled: {outcome:?}")
        };
        usage.stop();
        usage.drain().await;
        assert_eq!(balance.priced_micro_usd, Some(18));
        assert!(
            f.events("root-f")
                .iter()
                .any(|k| k == "FeatureBudgetWarning")
        );

        let agent = f
            .agent
            .clone()
            .with_lineage(
                Some(ProjectId(id("project-a"))),
                Some(RootId(id("root-f"))),
                None,
            )
            .with_session(session.clone());
        let inbox = RetainedInboxDriver::new(f.store.clone())
            .receive(
                agent,
                ProjectId(id("project-a")),
                InboxSelection {
                    recipient: Recipient::Session(session.clone()),
                    after: 0,
                    include_read: false,
                    limit: 10,
                    wait_seconds: 0,
                },
            )
            .await
            .unwrap();
        let warnings: Vec<_> = inbox
            .items
            .iter()
            .filter(|(_, message)| message.body.starts_with("Feature budget warning"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", inbox.items);
        let (delivery, message) = warnings[0];
        assert_eq!(delivery.recipient_session_id.as_ref(), Some(&session));
        assert_eq!(message.target_feature_id, Some(RootId(id("root-f"))));
        assert!(
            message.body.contains("reached 75% of its limit")
                && message.body.contains("Spent $0.000018 of $0.000024")
                && message.body.contains("$0.000006 remains"),
            "{}",
            message.body
        );
        // The latch delivers once: a later reconcile under the same cap adds no second copy.
        f.tx(|svc, tx| {
            let work = tx.work().work(&id("root-f"))?.unwrap();
            budget::reconcile(tx, svc, &work)
        });
        let copies = read(f.store.as_ref(), |s| {
            s.runs()
                .agent_session_inbox(&ProjectId(id("project-a")), &session, 0, true, 10)
        });
        assert_eq!(copies.len(), 1);
    }));
}
