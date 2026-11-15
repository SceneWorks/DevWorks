//! HTTP intents an event handler commits with `broker.commit_event` run once through their named
//! connection, as the person who approved the committing version and within its network grant,
//! journaled on the intent's operation; a restart never sends them again. An ungranted endpoint
//! is refused at commit, and a version no person approved runs none.
use super::event_intents::{commit_directly, drive, run_event};
use super::http_connections::{Names, SECRET, SECRET_REF, Secrets, Server, http, set_connections};
use super::*;
use devworks_core::protocol::OperationState;
use devworks_engine::package_host::http::NetworkPorts;

const CAPABILITY: &str = "package.broker.http";

fn attach(a: &App, server: &Arc<Server>) {
    a.app().packages().unwrap().attach_network(NetworkPorts {
        resolver: Arc::new(Names),
        transport: server.clone(),
        secrets: Arc::new(Secrets),
    });
}

/// A booted app with the test network, the `main` connection and a package installed by the
/// person whose manifest grants `permissions`; its name.
fn installed(a: &App, server: &Arc<Server>, tag: &str, permissions: Vec<Value>) -> String {
    attach(a, server);
    set_connections(
        &*a.store,
        json!([{"name": "main", "endpointId": "api", "baseUrl": "https://api.example.test/v1",
            "credential": {"secretRef": SECRET_REF}}]),
    );
    let name = format!("{tag}-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.permissions = permissions;
    package(a.install(&a.user, &spec));
    a.ready(&name);
    name
}

fn http_intents(a: &App) -> Vec<OperationRecord> {
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

fn settled(a: &App, state: OperationState) -> OperationRecord {
    let until = Instant::now() + PATIENCE;
    loop {
        let ops = http_intents(a);
        if ops.len() == 1 && ops[0].state == state {
            return ops[0].clone();
        }
        assert!(Instant::now() < until, "never {state:?}: {ops:?}");
        drive(a, Duration::from_millis(20));
    }
}

#[test]
fn a_committed_http_intent_runs_once_through_its_connection_as_the_approving_person() {
    let _s = serial();
    let mut a = App::new();
    let server = Arc::new(Server::default());
    let name = installed(
        &a,
        &server,
        "ehttp-pack",
        vec![
            json!({"kind": "StateWrite"}),
            json!({"kind": "Network", "endpointId": "api"}),
        ],
    );
    let body = a.save(&a.user, br#"{"event":"pushed"}"#);
    let intents = json!([{"actionKey": "notify",
        "call": http("main", "api", "Post", "/hooks", Some(body.clone()))}]);
    let progress = run_event(&a, &name, 1, &body, intents);
    assert!(
        progress[2].starts_with("commit:EventCommitAccepted"),
        "{progress:?}"
    );
    let done = settled(&a, OperationState::ObservedComplete);
    // Sent once, through the named connection, with the host's credential and the body.
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].target, "/v1/hooks");
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == SECRET)
    );
    // Journaled on the intent's operation: the committing contribution and version, and the
    // answer recorded on its attempt with the status and bytes.
    assert_eq!(done.attempts.len(), 1, "{done:?}");
    assert_eq!(done.payload["packageId"], json!(name));
    assert_eq!(
        done.payload["contributionId"],
        json!(format!("{name}/event"))
    );
    assert!(done.payload["manifestDigest"].as_str().is_some());
    let recorded = format!("{:?}", done.attempts);
    for field in [
        "\"result\"",
        "\"reply\"",
        "\"status\": Number(200)",
        "\"requestBytes\": Number(18)",
    ] {
        assert!(recorded.contains(field), "{field}: {recorded}");
    }
    assert!(!recorded.contains(SECRET));
    // The answer belongs to the approving person (the person who installed the version).
    let reply = done.attempts[0]
        .observations
        .iter()
        .find_map(|o| o.get("result").cloned())
        .expect("the recorded answer");
    let answer = super::broker::read_artifact(&a, &a.user, &serde_json::from_value(reply).unwrap());
    assert_eq!(answer["status"], 200);
    // A restart never sends it again.
    a.shutdown();
    a.boot();
    attach(&a, &server);
    a.ready(&name);
    drive(&a, Duration::from_millis(1500));
    let after = settled(&a, OperationState::ObservedComplete);
    assert_eq!(after.attempts.len(), 1);
    assert_eq!(server.seen.lock().unwrap().len(), 1);
}

#[test]
fn an_http_intent_to_an_ungranted_endpoint_is_refused_at_commit() {
    let _s = serial();
    let a = App::new();
    let server = Arc::new(Server::default());
    // The manifest grants no network access at all.
    let name = installed(
        &a,
        &server,
        "nonet-pack",
        vec![json!({"kind": "StateWrite"})],
    );
    let body = a.save(&a.user, b"{}");
    let intents = json!([{"actionKey": "notify",
        "call": http("main", "api", "Post", "/hooks", Some(body.clone()))}]);
    let progress = run_event(&a, &name, 1, &body, intents);
    assert_eq!(progress[2], "commit:PermissionDenied:-", "{progress:?}");
    assert!(http_intents(&a).is_empty());
    assert!(server.seen.lock().unwrap().is_empty());
}

#[test]
fn a_version_no_person_approved_runs_none_of_its_http_intents() {
    let _s = serial();
    let a = App::new();
    let server = Arc::new(Server::default());
    let name = installed(
        &a,
        &server,
        "trusted-pack",
        vec![
            json!({"kind": "StateWrite"}),
            json!({"kind": "Network", "endpointId": "api"}),
        ],
    );
    let digest = digest_of(&a.record(&name));
    // The version as an automatic trusted update would have activated it: no person approved it.
    let (pkg, d) = (name.clone(), digest.clone());
    write(&*a.store, move |tx| {
        tx.packages().update_version(
            &pkg,
            &d,
            VersionPhase::Installed,
            &[],
            Some(&json!({"kind": "TrustedPublisher", "publisherId": "acme"})),
        )
    })
    .unwrap();
    commit_directly(
        &a,
        &name,
        &digest,
        1,
        "event",
        json!([{"actionKey": "notify", "call": http("main", "api", "Get", "/hooks", None)}]),
    );
    let refused = settled(&a, OperationState::Failed);
    assert!(
        format!("{:?}", refused.attempts).contains("NoApprover"),
        "{refused:?}"
    );
    assert!(server.seen.lock().unwrap().is_empty(), "nothing was sent");
}
