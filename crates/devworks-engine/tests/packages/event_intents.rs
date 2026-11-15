//! The effect intents an event handler commits with `broker.commit_event` execute once, as the
//! person who approved the committing version, narrowed by its manifest grants; an intent in
//! flight when the host stopped is held for reconciliation, never executed again; an intent
//! outside the grant is refused and never executed.
use super::broker::{application, input_schema};
use super::*;
use devworks_core::protocol::OperationState;
use devworks_core::registry::Mode;
use devworks_engine::package::{IntentOrigin, commit_event_from};
use devworks_engine::package_host::intents::CONSUMER;
use devworks_extension_protocol::package::EventCommit;

const CAPABILITY: &str = "package.broker.application";

/// A Mutation-tool package and the package its intents disable, both installed by the person.
fn installed(a: &App, name: &str, effect: &'static str) {
    let mut spec = Spec::new(name, "1.0.0");
    spec.effect = effect;
    package(a.install(&a.user, &spec));
    package(a.install(&a.user, &Spec::new("target-pack", "1.0.0")));
    a.ready(name);
}

/// The arguments of `addons.disable` for `target-pack`, saved by the person.
fn disable_args(a: &App) -> Value {
    a.save(
        &a.user,
        json!({"command": {"kind": "Disable", "packageId": "target-pack",
            "expectedRegistryRevision": a.registry().to_string()}})
        .to_string()
        .as_bytes(),
    )
}

fn disable_intent(args: &Value) -> Value {
    json!([{"actionKey": "disable", "call": application("addons.disable",
        input_schema("addons.disable", Mode::Command, Some(2)), args.clone())}])
}

/// A claimed delivery of a subscription of `package` no consumer handles (the test drives it).
fn claimed(a: &App, package: &str, n: usize) -> QueueEntry {
    let consumer = format!("pkg-{package}-sub-{n}");
    enqueue(&*a.store, &consumer, None, json!({}));
    write(&*a.store, move |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: consumer.clone(),
            owner: id("package-host"),
            lease_ms: 60_000,
            limit: 1,
        })
    })
    .unwrap()
    .remove(0)
}

fn invocation(a: &App, claimed: &QueueEntry, n: usize) -> PackageInvocation {
    PackageInvocation {
        package_id: String::new(),
        subscription_id: id(&format!("sub-{n}")),
        process_session_id: String::new(),
        invocation_id: String::new(),
        invocation_token: String::new(),
        state_generation: 0,
        delivery_id: claimed.id.clone(),
        fence: claimed.fence().unwrap(),
        cancellation_epoch: claimed.cancellation_epoch,
        actor: a.store.info().host_actor,
        broker_consumer: CONSUMER.into(),
    }
}

/// The commit of `intents` for `claimed`, as the fixture's `event` contribution sends it.
fn commit(a: &App, package: &str, claimed: &QueueEntry, n: usize, intents: Value) -> Value {
    let pkg = package.to_owned();
    let state = read(&*a.store, move |s| s.packages().package_state(&pkg)).unwrap();
    json!({"subscriptionId": format!("sub-{n}"), "eventId": claimed.event_id.0.as_str(),
        "expectedCheckpoint": "0", "stateGeneration": state.generation.to_string(),
        "expectedStateRevision": state.revision.to_string(), "intents": intents})
}

/// Runs `package`'s `event` contribution on `input`, committing `intents`; its progress.
fn run_event(a: &App, package: &str, n: usize, input: &Value, intents: Value) -> Vec<String> {
    let delivery = claimed(a, package, n);
    let path = std::env::temp_dir().join(format!("devworks-fixture-{package}-commit.json"));
    std::fs::write(&path, commit(a, package, &delivery, n, intents).to_string()).unwrap();
    let h = a
        .app()
        .packages()
        .unwrap()
        .invoke(InvokeRequest {
            package_id: package.into(),
            contribution_id: format!("{package}/event"),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(input.clone()).unwrap(),
            operation_id: id(&a.key("op")),
            deadline: Duration::from_secs(30),
            allowed_operations: vec![],
            kind: InvocationKind::Event(Box::new(invocation(a, &delivery, n))),
            principal: None,
            expected_digest: None,
        })
        .unwrap();
    finished(h.wait(Duration::from_secs(20)));
    let _ = std::fs::remove_file(&path);
    h.progress().0
}

fn intents(a: &App) -> Vec<OperationRecord> {
    read(&*a.store, |s| {
        s.operations().operations_for_capability(
            CAPABILITY,
            &[
                OperationState::IntentRecorded,
                OperationState::Dispatching,
                OperationState::ObservedComplete,
                OperationState::Failed,
                OperationState::OutcomeUnknown,
            ],
            None,
            10,
        )
    })
}

/// Waits until the one intent reaches `state`.
fn settled(a: &App, state: OperationState) -> OperationRecord {
    let until = Instant::now() + PATIENCE;
    loop {
        let ops = intents(a);
        if ops.len() == 1 && ops[0].state == state {
            return ops[0].clone();
        }
        assert!(
            Instant::now() < until,
            "never {state:?}: {ops:?}\nqueue: {:?}",
            read(&*a.store, |s| s.queue().status(CONSUMER))
        );
        drive(a, Duration::from_millis(20));
    }
}

/// Lets the app's background loops (the dispatcher among them) run for `d`: the test runtime is
/// single-threaded and runs them only while blocked on.
fn drive(a: &App, d: Duration) {
    a.rt.block_on(async { tokio::time::sleep(d).await });
}

fn target_actors(a: &App) -> Vec<String> {
    read(&*a.store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: "target-pack".into(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .map(|e| format!("{}:{}", e.kind, e.actor.0.as_str()))
    .collect()
}

#[test]
fn a_committed_intent_runs_once_as_the_approving_person_and_never_again_after_restart() {
    let _s = serial();
    let mut a = App::new();
    installed(&a, "intent-pack", "Mutation");
    let args = disable_args(&a);
    let progress = run_event(&a, "intent-pack", 1, &args, disable_intent(&args));
    assert!(
        progress[2].starts_with("commit:EventCommitAccepted"),
        "{progress:?}"
    );
    let done = settled(&a, OperationState::ObservedComplete);
    assert_eq!(done.attempts.len(), 1, "{done:?}");
    assert_eq!(done.payload["contributionId"], "intent-pack/event");
    // It ran as the person who approved the version (who installed it), never as the package.
    assert!(!a.record("target-pack").enabled);
    let actors = target_actors(&a);
    assert!(
        actors.iter().any(|e| e.ends_with(":user-a"))
            && !actors.iter().any(|e| e.contains(":package-")),
        "{actors:?}"
    );
    // After a restart the completed intent is not executed again.
    package(a.simple(&a.user, "addons.enable", "Enable", "target-pack"));
    a.shutdown();
    a.boot();
    a.ready("intent-pack");
    drive(&a, Duration::from_millis(1500));
    let after = settled(&a, OperationState::ObservedComplete);
    assert_eq!(after.attempts.len(), 1, "{after:?}");
    assert!(a.record("target-pack").enabled);
    assert_eq!(read(&*a.store, |s| s.queue().status(CONSUMER)).ready, 0);
}

/// Commits `intents` for `package` directly, as `broker.commit_event` does, from `contribution`
/// of the version `digest` (without the commit-time grant check).
fn commit_directly(
    a: &App,
    package: &str,
    digest: &str,
    n: usize,
    contribution: &str,
    intents: Value,
) {
    let delivery = claimed(a, package, n);
    let mut inv = invocation(a, &delivery, n);
    inv.package_id = package.into();
    inv.process_session_id = "s-direct".into();
    inv.invocation_id = "i-direct".into();
    inv.invocation_token = "t-direct".into();
    let mut body = commit(a, package, &delivery, n, intents);
    body["processSessionId"] = json!("s-direct");
    body["invocationId"] = json!("i-direct");
    body["invocationToken"] = json!("t-direct");
    let pkg = package.to_owned();
    inv.state_generation = read(&*a.store, move |s| s.packages().package_state(&pkg))
        .unwrap()
        .generation;
    let commit: EventCommit = serde_json::from_value(body).unwrap();
    let origin = IntentOrigin {
        contribution_id: format!("{package}/{contribution}"),
        manifest_digest: digest.to_owned(),
    };
    let outcome = write(&*a.store, move |tx| {
        commit_event_from(tx, &inv, Some(&origin), &commit)
    });
    assert!(
        matches!(
            outcome,
            Ok(devworks_engine::package::CommitOutcome::Accepted(_))
        ),
        "{outcome:?}"
    );
}

#[test]
fn an_intent_in_flight_when_the_host_stopped_is_held_and_never_executed_again() {
    let _s = serial();
    let mut a = App::new();
    installed(&a, "flight-pack", "Mutation");
    let args = disable_args(&a);
    let digest = digest_of(&a.record("flight-pack"));
    a.shutdown();
    // What a dispatcher that crashed after starting the effect leaves: the intent's delivery
    // claimed and its operation `Dispatching`.
    commit_directly(
        &a,
        "flight-pack",
        &digest,
        1,
        "event",
        disable_intent(&args),
    );
    write(&*a.store, |tx| {
        let entry = tx
            .queue()
            .claim_ready(&ClaimRequest {
                consumer: CONSUMER.into(),
                owner: id("crashed-dispatcher"),
                lease_ms: 1,
                limit: 1,
            })?
            .remove(0);
        let op = tx
            .operations()
            .operation(entry.operation_id.as_ref().unwrap())?
            .unwrap();
        let attempt = tx.operations().begin_attempt(
            &op.id,
            op.revision,
            AttemptBinding {
                owner_generation: Some(entry.generation),
                ..AttemptBinding::default()
            },
        )?;
        tx.operations().transition(
            &op.id,
            op.revision + 1,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            Some(json!({"dispatcher": "crashed-dispatcher"})),
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(intents(&a)[0].payload["manifestDigest"], digest);
    // The host starts again after the crashed claim's lease expired. Recovery holds the delivery
    // for reconciliation: its operation may have dispatched, so it is never executed again.
    a.h.advance(60_000);
    a.boot();
    let until = Instant::now() + PATIENCE;
    while read(&*a.store, |s| s.queue().status(CONSUMER)).reconciling != 1 {
        assert!(
            Instant::now() < until,
            "{:?}",
            read(&*a.store, |s| s.queue().status(CONSUMER))
        );
        drive(&a, Duration::from_millis(20));
    }
    drive(&a, Duration::from_millis(1500));
    let held = intents(&a).remove(0);
    assert_eq!(held.state, OperationState::Dispatching, "{held:?}");
    assert_eq!(held.attempts.len(), 1, "{held:?}");
    assert!(a.record("target-pack").enabled, "the intent ran again");
}

#[test]
fn an_intent_outside_the_packages_grant_is_refused_and_never_executed() {
    let _s = serial();
    let a = App::new();
    // Every contribution of this package is a read-only tool.
    installed(&a, "readonly-pack", "ReadCandidate");
    let args = disable_args(&a);
    let before = read(&*a.store, |s| s.packages().package_state("readonly-pack")).unwrap();
    // The commit is refused as a whole: no state, receipt or intent is recorded.
    let progress = run_event(&a, "readonly-pack", 1, &args, disable_intent(&args));
    assert_eq!(progress[2], "commit:PermissionDenied:-", "{progress:?}");
    assert!(intents(&a).is_empty());
    let after = read(&*a.store, |s| s.packages().package_state("readonly-pack")).unwrap();
    assert_eq!(after.revision, before.revision);
    // An intent recorded for that contribution anyway is refused when it would execute.
    let digest = digest_of(&a.record("readonly-pack"));
    commit_directly(
        &a,
        "readonly-pack",
        &digest,
        2,
        "event",
        disable_intent(&args),
    );
    let failed = settled(&a, OperationState::Failed);
    let cause = format!("{:?}", failed);
    assert!(cause.contains("OutsideGrant"), "{failed:?}");
    assert!(a.record("target-pack").enabled);
}
