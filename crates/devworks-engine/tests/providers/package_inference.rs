//! A package's brokered inference runs as routed text inference of the invoking agent's
//! session, during that agent's dispatching `package.invoke` tool call, and is priced and
//! recorded as `Other` usage. Scripted provider bytes stand in for the model.
use super::mcp_sampling::{WIRE, agent_session};
use super::*;
use devworks_core::ports::AuthorityFence;
use devworks_core::protocol::{OperationId, OperationState};
use devworks_core::services::mcp::SamplingGrant;
use devworks_engine::package_host::inference::{
    PackageInferenceRequest, event_grant, grant_for, infer, is_person_session, person_grant,
    person_session_id,
};

const INVOCATION: &str = "package-tool-call";

/// The agent session plus one dispatching `package.invoke` call, as the package tool consumer
/// journals an agent's call of a package tool.
fn package_call(f: &Fx) -> OperationId {
    let (_, revision) = agent_session(f);
    let key = ToolCallKey {
        attempt_id: AttemptId(id("provider-attempt")),
        native_call_id: "call-package".into(),
        ordinal: Counter::new(0).unwrap(),
    };
    let fences = vec![AuthorityFence::Principal {
        actor: ActorId(id("sampling-agent")),
        revision,
    }];
    let payload = json!({"packageId": "summarizer", "contributionId": "summarizer/run",
        "principal": "sampling-agent", "principalKind": "Agent", "sessionId": "agent-session",
        "projectId": null, "rootId": null, "assignmentId": null, "fences": fences,
        "call": key, "activation": null});
    write(f.store.as_ref(), move |tx| {
        let operation = OperationId(id(INVOCATION));
        let intent = tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: "package.invoke".into(),
            schema_version: 1,
            args_digest: "b".repeat(64),
            payload,
        })?;
        let attempt = tx.operations().begin_attempt(
            &operation,
            intent.revision,
            AttemptBinding::default(),
        )?;
        let current = tx.operations().operation(&operation)?.unwrap();
        tx.operations().transition(
            &operation,
            current.revision,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )?;
        Ok(())
    })
    .unwrap();
    OperationId(id(INVOCATION))
}

fn request(text: &str) -> PackageInferenceRequest {
    PackageInferenceRequest::parse(
        json!({"messages": [{"role": "user", "text": text}], "systemPrompt": "Answer in one line.",
            "maxTokens": 64})
        .to_string()
        .as_bytes(),
    )
    .expect("a request in the host schema")
}

fn requests(f: &Fx) -> usize {
    f.http.lock().unwrap().requests.lock().unwrap().len()
}

#[test]
fn package_inference_runs_in_the_invoking_agents_session_as_other_usage() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        let operation = package_call(&f);
        let router = Arc::new(InferenceRouter::new(
            f.h.ids.clone(),
            f.h.clock(),
            f.connections.clone(),
        ));
        let grant = grant_for(f.store.as_ref(), &operation).expect("the agent's tool call");
        let run = |text: &'static str, action: &'static str| {
            let (store, artifacts, driver, router, grant) = (
                f.store.clone(),
                f.artifacts.clone(),
                f.driver.clone(),
                router.clone(),
                grant.clone(),
            );
            async move {
                infer(
                    store,
                    artifacts,
                    driver,
                    router,
                    &grant,
                    action,
                    &request(text),
                    None,
                )
                .await
            }
        };
        let answer = run("Summarize the change", "summarize")
            .await
            .expect("routed package inference");
        assert_eq!(answer["text"], "Sampled answer");
        assert_eq!(answer["model"], "model");
        assert_eq!(answer["truncated"], false);
        assert_eq!(requests(&f), 1);
        let body = String::from_utf8(
            f.http.lock().unwrap().requests.lock().unwrap()[0]
                .body
                .clone()
                .unwrap(),
        )
        .unwrap();
        assert!(
            body.contains("Summarize the change") && body.contains("installed DevWorks add-on")
        );

        // Priced usage of the agent's session, reported as Other and attributed to the
        // invocation's tool call.
        let (category, attempt, route_purpose) = read(f.store.as_ref(), |s| {
            let attempts = s.usage().inference_attempts(
                &devworks_core::ports::storage::UsageAttemptFilter::default(),
                None,
                64,
            )?;
            let attempt = attempts.into_iter().next().unwrap();
            let route = s.router().route_request(&attempt.request_id)?.unwrap();
            let journal = s.operations().operation(&route.operation_id)?.unwrap();
            Ok((
                journal.payload["reportCategory"].clone(),
                attempt,
                route.purpose,
            ))
        });
        assert_eq!(category, "Other");
        assert_eq!(route_purpose, "Enrichment");
        assert_eq!(attempt.session_id, Some(id("agent-session")));
        assert_eq!(attempt.actor_id, ActorId(id("sampling-agent")));
        assert_eq!(attempt.state, "Settled");
        assert!(attempt.request_id.as_str().starts_with("pkgi-"));

        // A retried call under the same action key returns the recorded answer, never sending
        // again; another action key is another request.
        let replay = run("Summarize the change", "summarize").await.unwrap();
        assert_eq!(replay["text"], "Sampled answer");
        assert_eq!(requests(&f), 1);

        // Once the agent's tool call settled, the invocation has nothing to account against.
        write(f.store.as_ref(), |tx| {
            let operation = OperationId(id(INVOCATION));
            let current = tx.operations().operation(&operation)?.unwrap();
            let fence = current.latest_attempt().unwrap().fence();
            tx.operations().transition(
                &operation,
                current.revision,
                Some(&fence),
                OperationState::ObservedComplete,
                None,
            )?;
            Ok(())
        })
        .unwrap();
        assert!(grant_for(f.store.as_ref(), &operation).is_err());
        assert!(run("After the call", "later").await.is_err());
        assert_eq!(requests(&f), 1);
    });
}

#[test]
fn a_request_outside_the_schema_or_its_bounds_is_refused_before_transport() {
    for bad in [
        json!({"messages": [], "maxTokens": 64}),
        json!({"messages": [{"role": "system", "text": "x"}], "maxTokens": 64}),
        json!({"messages": [{"role": "user", "text": "x"}], "maxTokens": 100000}),
        json!({"messages": [{"role": "user", "text": "x"}], "maxTokens": 64, "tools": []}),
    ] {
        let parsed = PackageInferenceRequest::parse(bad.to_string().as_bytes());
        // Unknown members never parse; the rest parse but their bounds refuse them.
        if let Some(parsed) = parsed {
            runtime().block_on(async {
                let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
                let operation = package_call(&f);
                let router = Arc::new(InferenceRouter::new(
                    f.h.ids.clone(),
                    f.h.clock(),
                    f.connections.clone(),
                ));
                let grant = grant_for(f.store.as_ref(), &operation).unwrap();
                let refused = infer(
                    f.store.clone(),
                    f.artifacts.clone(),
                    f.driver.clone(),
                    router,
                    &grant,
                    "bad",
                    &parsed,
                    None,
                )
                .await;
                assert!(refused.is_err(), "{bad}");
                assert_eq!(requests(&f), 0);
            });
        }
    }
}

// A package invocation a person or an event handler started has no agent session: it runs as a
// person in that person's package inference session.

/// Makes the fixture model the High tier pool that sessions without a pin route through.
fn route_high_tier_to_the_fixture_model(f: &Fx) {
    write(f.store.as_ref(), |tx| {
        let key = "routing.tierHigh.members";
        let schema = devworks_core::settings::catalog::descriptor(key)
            .unwrap()
            .schema_ref();
        tx.configuration().append_revision(
            &devworks_core::ports::storage::ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values": {key: {"schema": schema, "value": ["model"]}}}),
        )?;
        Ok(())
    })
    .unwrap();
}

/// The registered person `name`'s own context for the invocation `invocation`.
fn invocation_of(f: &Fx, name: &str, invocation: &str) -> InvocationContext {
    let info = f.store.info();
    devworks_core::context::HostIngress::new(f.store.as_ref()).context(
        ActorId(id(name)),
        PrincipalKind::Human,
        id(invocation),
        id("package-inference"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}

async fn answer(f: &Fx, grant: &SamplingGrant, action: &'static str, text: &str) -> Option<Value> {
    let router = Arc::new(InferenceRouter::new(
        f.h.ids.clone(),
        f.h.clock(),
        f.connections.clone(),
    ));
    infer(
        f.store.clone(),
        f.artifacts.clone(),
        f.driver.clone(),
        router,
        grant,
        action,
        &request(text),
        None,
    )
    .await
    .ok()
}

/// The one recorded attempt: (report category, route purpose, attempt).
fn recorded(
    f: &Fx,
) -> (
    Value,
    String,
    devworks_core::ports::storage::StoredInferenceAttempt,
) {
    read(f.store.as_ref(), |s| {
        let attempts = s.usage().inference_attempts(
            &devworks_core::ports::storage::UsageAttemptFilter::default(),
            None,
            64,
        )?;
        assert_eq!(attempts.len(), 1);
        let attempt = attempts.into_iter().next().unwrap();
        let route = s.router().route_request(&attempt.request_id)?.unwrap();
        let journal = s.operations().operation(&route.operation_id)?.unwrap();
        Ok((
            journal.payload["reportCategory"].clone(),
            route.purpose,
            attempt,
        ))
    })
}

#[test]
fn a_person_started_invocation_infers_as_that_person_in_their_package_session() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        route_high_tier_to_the_fixture_model(&f);
        let person = principal(f.store.as_ref(), "package-person", PrincipalKind::Human);
        let invocation = OperationId(person.invocation_id().clone());
        let grant = person_grant(f.store.as_ref(), &person, &invocation).expect("a person's grant");

        let reply = answer(&f, &grant, "summarize", "Summarize the change")
            .await
            .expect("a model result for the person's invocation");
        assert_eq!(reply["text"], "Sampled answer");
        assert_eq!(requests(&f), 1);

        let (category, purpose, attempt) = recorded(&f);
        let session = person_session_id(&ActorId(id("package-person")), None, None);
        assert_eq!(category, "Other");
        assert_eq!(purpose, "Enrichment");
        assert_eq!(attempt.actor_id, ActorId(id("package-person")));
        assert_eq!(attempt.session_id, Some(session.clone()));
        assert_eq!(attempt.state, "Settled");
        assert!(attempt.request_id.as_str().starts_with("pkgi-"));
        let session = read(f.store.as_ref(), |s| s.sessions().session(&session));
        assert!(session.is_some_and(|s| is_person_session(&s, &ActorId(id("package-person")))));

        // The same call again returns the recorded answer; a second invocation of the same
        // person reuses the session and is its own request.
        let replay = answer(&f, &grant, "summarize", "Summarize the change").await;
        assert_eq!(replay.unwrap()["text"], "Sampled answer");
        assert_eq!(requests(&f), 1);
        let second = invocation_of(&f, "package-person", "second-invocation");
        let other = person_grant(
            f.store.as_ref(),
            &second,
            &OperationId(id("second-invocation")),
        )
        .unwrap();
        assert_eq!(other.context.session_id(), grant.context.session_id());
        // The scripted provider serves one body per script.
        f.http
            .lock()
            .unwrap()
            .body
            .push_back(WIRE.as_bytes().to_vec());
        assert!(answer(&f, &other, "summarize", "Again").await.is_some());
        assert_eq!(requests(&f), 2);
    });
}

#[test]
fn an_agent_cannot_use_the_person_path_and_a_person_cannot_borrow_an_agent_session() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        route_high_tier_to_the_fixture_model(&f);
        let agent = principal(f.store.as_ref(), "package-agent", PrincipalKind::Agent);
        let invocation = OperationId(agent.invocation_id().clone());
        assert!(person_grant(f.store.as_ref(), &agent, &invocation).is_err());

        // A person's grant naming the agent's session is refused before any provider request.
        let (_, _) = super::mcp_sampling::agent_session(&f);
        let person = principal(f.store.as_ref(), "package-person", PrincipalKind::Human);
        let invocation = OperationId(person.invocation_id().clone());
        let mut grant = person_grant(f.store.as_ref(), &person, &invocation).unwrap();
        grant.context = grant.context.with_session(id("agent-session"));
        assert!(answer(&f, &grant, "borrow", "x").await.is_none());
        assert_eq!(requests(&f), 0);
    });
}

/// Installs `package` with one version whose approval is `approval`.
fn install(f: &Fx, package: &str, digest: &str, approval: Option<Value>) {
    use devworks_core::ports::storage::{StoredPackage, StoredPackageVersion, VersionPhase};
    let (package, digest) = (package.to_owned(), digest.to_owned());
    write(f.store.as_ref(), move |tx| {
        tx.packages().insert_package(&StoredPackage {
            package_id: package.clone(),
            revision: 1,
            enabled: true,
            installed: true,
            active_digest: Some(digest.clone()),
            predecessor_digest: None,
            pending_digest: None,
            state_generation: 1,
            config: None,
            config_revision: 1,
            trust: None,
            failure: None,
            created_at_ms: T0,
            updated_at_ms: T0,
        })?;
        tx.packages().insert_version(&StoredPackageVersion {
            package_id: package,
            manifest_digest: digest,
            bundle_digest: "b".repeat(64),
            version: "1.0.0".into(),
            publisher_id: "publisher".into(),
            signer_key: None,
            inspection: json!({}),
            provenance: json!({}),
            phase: VersionPhase::Installed,
            waits: vec![],
            approval,
            created_at_ms: T0,
            updated_at_ms: T0,
        })?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn an_event_handler_infers_as_the_person_who_approved_its_exact_version() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        route_high_tier_to_the_fixture_model(&f);
        principal(f.store.as_ref(), "approving-person", PrincipalKind::Human);
        let digest = "a".repeat(64);
        install(
            &f,
            "summarizer",
            &digest,
            Some(json!({"by": "approving-person", "kind": "Person", "atMs": T0})),
        );
        let invocation = OperationId(id("event-invocation"));
        let grant = event_grant(f.store.as_ref(), "summarizer", &digest, &invocation)
            .expect("the approving person's grant");
        assert_eq!(grant.context.principal(), &ActorId(id("approving-person")));
        let reply = answer(&f, &grant, "summarize", "Summarize the event")
            .await
            .expect("a model result for the event handler");
        assert_eq!(reply["text"], "Sampled answer");
        assert_eq!(requests(&f), 1);
        let (category, _, attempt) = recorded(&f);
        assert_eq!(category, "Other");
        assert_eq!(attempt.actor_id, ActorId(id("approving-person")));
        assert_eq!(
            attempt.session_id,
            Some(person_session_id(
                &ActorId(id("approving-person")),
                None,
                None
            ))
        );
        assert_eq!(attempt.state, "Settled");
    });
}

#[test]
fn an_event_handler_of_a_version_no_person_approved_is_refused_without_a_request() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        route_high_tier_to_the_fixture_model(&f);
        principal(f.store.as_ref(), "trusted-agent", PrincipalKind::Agent);
        for (package, approval) in [
            ("trusted", Some(json!({"by": "publisher", "kind": "Trust"}))),
            ("plain", None),
            (
                "agent-approved",
                Some(json!({"by": "trusted-agent", "kind": "Person"})),
            ),
        ] {
            let digest = "c".repeat(64);
            install(&f, package, &digest, approval);
            let refused = event_grant(
                f.store.as_ref(),
                package,
                &digest,
                &OperationId(id("event-invocation")),
            );
            assert!(refused.is_err(), "{package}");
        }
        // An unknown package or version has no approver either.
        assert!(
            event_grant(
                f.store.as_ref(),
                "missing",
                &"d".repeat(64),
                &OperationId(id("event-invocation"))
            )
            .is_err()
        );
        assert_eq!(requests(&f), 0);
    });
}

fn project(f: &Fx, name: &str, lifecycle: &str, revision: i64) {
    use devworks_core::ports::storage::StoredProject;
    let (name, lifecycle) = (name.to_owned(), lifecycle.to_owned());
    write(f.store.as_ref(), move |tx| {
        let stored = StoredProject {
            id: devworks_core::protocol::ProjectId(id(&name)),
            name: name.clone(),
            description: String::new(),
            lifecycle: lifecycle.clone(),
            revision,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        };
        if revision == 1 {
            tx.projects().insert_project(&stored)
        } else {
            tx.projects().update_project(&stored, revision - 1)
        }
    })
    .unwrap();
}

#[test]
fn a_person_who_lost_the_project_is_refused_without_a_provider_request() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        route_high_tier_to_the_fixture_model(&f);
        let person = principal(f.store.as_ref(), "package-person", PrincipalKind::Human);
        let invocation = OperationId(person.invocation_id().clone());
        let scoped = person.with_lineage(
            Some(devworks_core::protocol::ProjectId(id("project-p"))),
            None,
            None,
        );

        // No such project: nothing opens, nothing is sent.
        assert!(person_grant(f.store.as_ref(), &scoped, &invocation).is_err());
        project(&f, "project-p", "Active", 1);
        let grant =
            person_grant(f.store.as_ref(), &scoped, &invocation).expect("an active project");
        assert!(answer(&f, &grant, "first", "Summarize").await.is_some());
        assert_eq!(requests(&f), 1);

        // The project is archived: the next call is refused before any provider request.
        project(&f, "project-p", "Archived", 2);
        assert!(person_grant(f.store.as_ref(), &scoped, &invocation).is_err());
        assert_eq!(requests(&f), 1);
    });
}

#[test]
fn a_context_that_is_not_the_invocations_own_cannot_spend_a_persons_budget() {
    let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
    let person = principal(f.store.as_ref(), "package-person", PrincipalKind::Human);
    assert!(
        person_grant(
            f.store.as_ref(),
            &person,
            &OperationId(id("another-invocation"))
        )
        .is_err()
    );
    assert_eq!(requests(&f), 0);
}

#[test]
fn an_event_handler_whose_approval_was_revoked_is_refused_without_a_request() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        route_high_tier_to_the_fixture_model(&f);
        principal(f.store.as_ref(), "approving-person", PrincipalKind::Human);
        let digest = "a".repeat(64);
        install(
            &f,
            "summarizer",
            &digest,
            Some(json!({"by": "approving-person", "kind": "Person", "atMs": T0})),
        );
        let invocation = OperationId(id("event-invocation"));
        let grant = event_grant(f.store.as_ref(), "summarizer", &digest, &invocation).unwrap();
        assert!(answer(&f, &grant, "first", "Summarize").await.is_some());
        assert_eq!(requests(&f), 1);

        // The approval is withdrawn: the next call is refused before any provider request.
        let withdrawn = digest.clone();
        write(f.store.as_ref(), move |tx| {
            tx.packages().update_version(
                "summarizer",
                &withdrawn,
                devworks_core::ports::storage::VersionPhase::Installed,
                &[],
                None,
            )
        })
        .unwrap();
        assert!(event_grant(f.store.as_ref(), "summarizer", &digest, &invocation).is_err());
        assert_eq!(requests(&f), 1);
    });
}
