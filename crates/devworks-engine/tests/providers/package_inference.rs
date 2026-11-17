//! A package's brokered inference runs as routed text inference of the invoking agent's
//! session, during that agent's dispatching `package.invoke` tool call, and is priced and
//! recorded as `Other` usage. Scripted provider bytes stand in for the model.
use super::mcp_sampling::{WIRE, agent_session};
use super::*;
use devworks_core::ports::AuthorityFence;
use devworks_core::protocol::{OperationId, OperationState};
use devworks_engine::package_host::inference::{PackageInferenceRequest, grant_for, infer};

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
        assert_eq!(
            attempt.state,
            devworks_core::ports::storage::AttemptState::Settled
        );
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
