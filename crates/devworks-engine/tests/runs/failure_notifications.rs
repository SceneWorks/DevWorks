//! Failures and authentication problems notify the user once per cause (NOTIFY-03).
use super::*;
use devworks_core::protocol::provider::EndpointProfile;
use devworks_core::providers::ProviderAccessBasis;
use devworks_core::services::router::connections::record_inference_rejection;

const USER: &str = "user-a";

fn notifications(fx: &Fx, category: &str) -> Vec<StoredNotification> {
    let category = category.to_owned();
    read(&*fx.store, move |s| {
        s.decisions().notifications(
            &NotificationFilter {
                user_id: ActorId(id(USER)),
                scope: ConfigScope::Workspace,
                categories: vec![category],
                read: None,
                unacknowledged_only: false,
                completion_only: false,
            },
            None,
            50,
        )
    })
}
fn bind_user(fx: &Fx) {
    fx.tx(|_, tx| tx.decisions().bind_notification_user(&ActorId(id(USER))));
}
fn links(n: &StoredNotification) -> Vec<(String, String)> {
    n.links
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["kind"].as_str().unwrap().to_owned(),
                l["targetId"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// A provider that refuses every request's credential.
struct RejectingWire(Arc<AtomicUsize>);
impl StreamingHttp for RejectingWire {
    fn open(
        &self,
        _: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 401,
                headers: vec![("content-type".into(), "application/json".into())],
            },
            body: Some(
                br#"{"error":{"type":"authentication_error","message":"invalid credential"}}"#
                    .to_vec(),
            ),
        }))
    }
}

const CONNECTION: &str = "rejected-connection";
const ENDPOINT: &str = "http://localhost:11434/v1";

fn basis(credential_revision: i64) -> ProviderAccessBasis {
    ProviderAccessBasis {
        connection_id: id(CONNECTION),
        profile: EndpointProfile::CompatibleChat,
        endpoint_digest: devworks_core::fingerprint::sha256_hex(ENDPOINT.as_bytes()),
        credential_revision,
        account_revision: 1,
        oauth: false,
    }
}
fn credential_holds(fx: &Fx, root: &str) -> Vec<StoredBlocker> {
    fx.holds(root)
        .into_iter()
        .filter(|b| b.cause == HoldCause::Credentials)
        .collect()
}

#[test]
fn a_provider_rejecting_the_credential_holds_the_root_and_notifies_once_per_credential() {
    struct Entropy;
    impl devworks_core::ports::http::Entropy for Entropy {
        fn fill(&self, bytes: &mut [u8]) {
            bytes.fill(5);
        }
    }
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    bind_user(&fx);
    let rt = runtime();
    rt.block_on(async {
        let mut composed = composition(&fx, None, Arc::new(FakePower::default()));
        let connections = Arc::new(
            devworks_core::services::router::connections::ConnectionService::new(
                fx.h.ids.clone(),
                fx.h.clock(),
                fx.store.clone(),
                Arc::new(devworks_core::ports::http::NoNetwork),
                Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
                Arc::new(Entropy),
            ),
        );
        composed.use_cases = UseCases::new()
            .with_connections(connections.clone())
            .with_router(Arc::new(
                devworks_core::services::router::InferenceRouter::new(
                    fx.h.ids.clone(),
                    fx.h.clock(),
                    connections,
                ),
            ));
        let wire_calls = Arc::new(AtomicUsize::new(0));
        composed.config.providers = Some(
            devworks_engine::providers::driver::ProviderComposition::new(Arc::new(RejectingWire(
                wire_calls.clone(),
            ))),
        );
        let app = Arc::new(boot(composed).await.unwrap());
        applied(&fx.start(&fx.user, "root-i", 1, 1));
        let activation = fx.launch("credential-owner");
        let host = principal(&*fx.store, "credential-host", PrincipalKind::Host);
        let agents = AgentService::new(fx.h.ids.clone(), fx.service.clone());
        let (execution_id, epoch) = (activation.id.clone(), activation.owner_epoch);
        let primary = fx.tx(move |_, tx| {
            agents
                .materialize_primary(tx, &host, &execution_id, epoch)
                .map_err(|error| StorageError::InvalidRecord(format!("primary: {error:?}")))
        });
        let assignment = read(&*fx.store, |s| s.runs().assignment(&primary.assignment_id)).unwrap();
        let session = read(&*fx.store, |s| s.sessions().session(&assignment.session_id)).unwrap();
        let context = devworks_core::context::HostIngress::new(&*fx.store)
            .context(
                session.agent_id.clone(),
                PrincipalKind::Agent,
                id("credential-invocation"),
                id("credential-ingress"),
                devworks_core::protocol::Counter::new(fx.store.info().restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_session(session.id.clone())
            .with_lineage(
                session.project_id.clone(),
                session.root_id.clone(),
                Some(primary.assignment_id.clone()),
            )
            .with_activation(activation.id.clone(), epoch);
        let pin = StoredPin {
            session_id: session.id.clone(),
            pin_id: id("credential-pin"),
            revision: 1,
            model_id: id("credential-model"),
            connection_id: id(CONNECTION),
            upstream_model_id: Some("fixture-model".into()),
            endpoint_profile: Some("CompatibleChat".into()),
            identity_origin: "Resolved".into(),
            intent: serde_json::to_string(&devworks_core::protocol::records::RouteIntent::Model {
                model_id: id("credential-model"),
            })
            .unwrap(),
            requested_effort: "Default".into(),
            effective_effort: "Default".into(),
            required_capabilities: vec!["tools".into()],
            replaced_pin_id: None,
            reason: "Initial".into(),
            updated_at_ms: 0,
        };
        let route = devworks_core::services::router::choice_of(&pin).unwrap();
        fx.tx(move |_, tx| {
            tx.router().put_connection(
                &StoredConnection {
                    id: id(CONNECTION),
                    name: "Team gateway".into(),
                    profile: "CompatibleChat".into(),
                    endpoint: ENDPOINT.into(),
                    enabled: true,
                    local_free: true,
                    open_router: None,
                    credential_ref: None,
                    credential_revision: 0,
                    credential_epoch: None,
                    lifecycle: "Connected".into(),
                    state_reason: None,
                    account_revision: 1,
                    refresh_operation_id: None,
                    account_digest: None,
                    access_expires_at_ms: None,
                    risk: None,
                    discovered: None,
                    discovered_at_ms: None,
                    healthy: Some(true),
                    checked_at_ms: Some(0),
                    revision: 1,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                },
                None,
            )?;
            tx.router().put_route_model(
                &StoredRouteModel {
                    id: id("credential-model"),
                    display_name: "Fixture model".into(),
                    endpoint_profile: "CompatibleChat".into(),
                    upstream_model_id: "fixture-model".into(),
                    enabled: true,
                    efforts: vec![],
                    image_input: false,
                    tools: true,
                    context_tokens: Some(16_000),
                    connection_id: Some(id(CONNECTION)),
                    position: 1,
                    revision: 1,
                    updated_at_ms: 0,
                },
                None,
            )?;
            tx.router().put_session_pin(&pin, None)?;
            Ok(())
        });
        let (head, binding) =
            devworks_engine::agent_turns::publish_first_context(&app, &context, &primary, &route)
                .unwrap();
        let (turn_session, assignment_id, turn_execution) = (
            session.id.clone(),
            primary.assignment_id.clone(),
            activation.id.clone(),
        );
        fx.tx(move |service, tx| {
            service
                .begin_turn(tx, &turn_session, &assignment_id, &turn_execution, epoch)?
                .map_err(|fence| StorageError::InvalidRecord(format!("turn: {fence:?}")))
        });
        let planned =
            devworks_engine::agent_turns::plan_generation(&app, &context, &route, &binding, &head)
                .unwrap();
        let progress = app.produce_and_dispatch_native_turn(planned, None).await;
        assert_eq!(wire_calls.load(Ordering::SeqCst), 1);
        if let Ok(progress) = &progress {
            assert!(
                progress
                    .provider
                    .result
                    .faults
                    .iter()
                    .any(|f| f.starts_with("Authentication:")),
                "{:?}",
                progress.provider.result.faults
            );
        }

        // One hold on the rejected root, naming the connection, and one card naming it.
        let connection = read(&*fx.store, |s| s.router().connection(&id(CONNECTION))).unwrap();
        assert_eq!(connection.lifecycle, "ReauthenticationRequired");
        let holds = credential_holds(&fx, "root-i");
        assert_eq!(holds.len(), 1, "{holds:?}");
        assert_eq!(holds[0].origin_id, CONNECTION);
        assert!(
            holds[0].reason.contains("Team gateway"),
            "{}",
            holds[0].reason
        );
        let cards = notifications(&fx, "Credentials");
        assert_eq!(cards.len(), 1);
        assert!(
            cards[0].title.contains("Team gateway"),
            "{}",
            cards[0].title
        );
        assert!(cards[0].requires_input);
        assert_eq!(
            links(&cards[0]),
            vec![("Connection".to_owned(), CONNECTION.to_owned())]
        );

        // A late rejection of the same credential (another in-flight request) adds nothing.
        fx.tx(|_, tx| record_inference_rejection(tx, &basis(0), 401, Some(&rid("root-i"))));
        assert_eq!(credential_holds(&fx, "root-i").len(), 1);
        assert_eq!(notifications(&fx, "Credentials").len(), 1);

        // The failed turn waits on the hold instead of escalating it as a second failure.
        let (x, e) = (activation.id.clone(), epoch);
        let waiting = fx.tx(move |svc, tx| {
            Ok(svc
                .report_failure(
                    tx,
                    &x,
                    e,
                    ActivationFailure::Deterministic("agent inference failed".into()),
                )?
                .unwrap())
        });
        assert_eq!(waiting.phase, ActivationPhase::Pending);
        assert!(
            fx.holds("root-i")
                .iter()
                .all(|b| b.cause != HoldCause::Policy)
        );
        assert!(notifications(&fx, "Failure").is_empty());
        assert!(
            fx.tx(|svc, tx| svc.claim_activation(tx, &id("credential-owner")))
                .is_none(),
            "a held root is not claimed"
        );

        // Signing in again (a new credential) clears the hold and the work becomes claimable.
        fx.tx(|_, tx| {
            let mut c = tx.router().connection(&id(CONNECTION))?.unwrap();
            let expected = c.revision;
            c.lifecycle = "Connected".into();
            c.credential_revision = 1;
            tx.router().put_connection(&c, Some(expected)).map(|_| ())
        });
        let swept = fx.tx(|svc, tx| svc.sweep_expired_waits(tx));
        assert_eq!(swept, vec![rid("root-i")]);
        assert!(credential_holds(&fx, "root-i").is_empty());
        assert!(
            fx.tx(|svc, tx| svc.claim_activation(tx, &id("credential-owner")))
                .is_some()
        );

        // A rejection of the new credential is a new cause: one more card and hold.
        fx.tx(|_, tx| record_inference_rejection(tx, &basis(1), 403, Some(&rid("root-i"))));
        assert_eq!(credential_holds(&fx, "root-i").len(), 1);
        assert_eq!(notifications(&fx, "Credentials").len(), 2);
    });
}

#[test]
fn a_root_that_fails_deterministically_notifies_once_linking_the_work() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    bind_user(&fx);
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let a = fx.launch("supervisor-1");
    // A transient failure that will be retried is not announced.
    let (x, e) = (a.id.clone(), a.owner_epoch);
    fx.tx(move |svc, tx| {
        svc.report_failure(
            tx,
            &x,
            e,
            ActivationFailure::Transient("connection lost".into()),
        )
    })
    .unwrap();
    assert!(notifications(&fx, "Failure").is_empty());
    fx.h.advance(fx.activation(&a.id).ready_at_ms - fx.h.now());
    let a = fx.launch("supervisor-1");
    let (x, e) = (a.id.clone(), a.owner_epoch);
    fx.tx(move |svc, tx| {
        svc.report_failure(
            tx,
            &x,
            e,
            ActivationFailure::Deterministic("the build script is missing".into()),
        )
    })
    .unwrap();
    let cards = notifications(&fx, "Failure");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].root_id, Some(rid("root-i")));
    assert!(cards[0].requires_input);
    assert!(cards[0].body.contains("the build script is missing"));
    assert_eq!(
        links(&cards[0]),
        vec![("Issue".to_owned(), "root-i".to_owned())]
    );
    // The same failure reported again is fenced and announced no second time.
    let (x, e) = (a.id.clone(), a.owner_epoch);
    assert!(
        fx.tx(move |svc, tx| svc.report_failure(
            tx,
            &x,
            e,
            ActivationFailure::Deterministic("the build script is missing".into())
        ))
        .is_err()
    );
    assert_eq!(notifications(&fx, "Failure").len(), 1);
}

#[test]
fn a_failed_assignment_notifies_once_linking_its_work_and_session() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    bind_user(&fx);
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let asg = fx.assign("root-f", "task-f1", None, None);
    let (assignment, revision) = (asg.id.clone(), asg.revision);
    let failed = fx.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        Ok(svc
            .set_assignment_state(
                tx,
                &host,
                &assignment,
                revision,
                AssignmentState::Failed,
                None,
            )?
            .unwrap())
    });
    assert_eq!(failed.state, AssignmentState::Failed);
    let cards = notifications(&fx, "Failure");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].root_id, Some(rid("root-f")));
    assert!(!cards[0].requires_input);
    assert_eq!(
        links(&cards[0]),
        vec![
            ("Task".to_owned(), "task-f1".to_owned()),
            ("Session".to_owned(), asg.session_id.as_str().to_owned()),
        ]
    );
    // An ended assignment cannot fail again.
    let (assignment, revision) = (asg.id.clone(), failed.revision);
    assert!(
        fx.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            svc.set_assignment_state(
                tx,
                &host,
                &assignment,
                revision,
                AssignmentState::Failed,
                None,
            )
        })
        .is_err()
    );
    assert_eq!(notifications(&fx, "Failure").len(), 1);
}
