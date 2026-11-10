//! Routed MCP sampling runs in the calling agent's session as detached Mcp usage. Scripted
//! provider bytes stand in for the model.
use super::*;
use devworks_core::ports::AuthorityFence;
use devworks_core::ports::mcp::{SamplingCall, SamplingMessage, SamplingRefusal};
use devworks_core::protocol::{OperationId, OperationState};
use devworks_core::services::mcp::SamplingGrant;
use devworks_engine::mcp_sampling::McpSampling;

const WIRE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Sampled answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn call(ordinal: u8, text: &str) -> SamplingCall {
    SamplingCall {
        ordinal,
        messages: vec![SamplingMessage {
            assistant: false,
            text: text.into(),
        }],
        system_prompt: Some("Answer in one line.".into()),
        max_tokens: 64,
        temperature: Some(0.2),
        stop_sequences: vec!["END".into()],
    }
}

fn pin(revision: i64) -> StoredPin {
    StoredPin {
        session_id: id("agent-session"),
        pin_id: id("agent-pin"),
        revision,
        model_id: id("model"),
        connection_id: id("connection"),
        upstream_model_id: Some("fixture-model".into()),
        endpoint_profile: Some("CompatibleChat".into()),
        identity_origin: "Resolved".into(),
        intent: serde_json::to_string(&devworks_core::protocol::records::RouteIntent::Model {
            model_id: id("model"),
        })
        .unwrap(),
        requested_effort: "Default".into(),
        effective_effort: "Default".into(),
        required_capabilities: vec![],
        replaced_pin_id: None,
        reason: "Initial".into(),
        updated_at_ms: T0,
    }
}

/// An agent session with a routed pin, a published context head and one dispatching
/// `mcp.tool` call, as the core service grants it to a sampling request.
fn agent_call(f: &Fx) -> SamplingGrant {
    let human = principal(f.store.as_ref(), "sampling-owner", PrincipalKind::Human);
    let agent = principal(f.store.as_ref(), "sampling-agent", PrincipalKind::Agent)
        .with_session(id("agent-session"));
    let head = f.bytes(b"{\"published\":\"head\"}");
    let key = ToolCallKey {
        attempt_id: AttemptId(id("provider-attempt")),
        native_call_id: "call-sample".into(),
        ordinal: Counter::new(0).unwrap(),
    };
    let payload = json!({"principal":"sampling-agent","principalKind":"Agent",
        "sessionId":"agent-session","call":key});
    let (revision, attempt) = write(f.store.as_ref(), move |tx| {
        let binding = bind_principal(
            tx,
            &human,
            NewPrincipalBinding {
                actor: ActorId(id("sampling-agent")),
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                project_id: None,
                tool_families: None,
                bound_by: ActorId(id("sampling-owner")),
            },
            None,
        )
        .map_err(|error| StorageError::InvalidRecord(format!("binding: {error:?}")))?;
        tx.sessions().insert_session(&StoredSession {
            id: id("agent-session"),
            conversation_id: id("agent-conversation"),
            agent_id: ActorId(id("sampling-agent")),
            project_id: None,
            root_id: None,
            title: "Sampling agent".into(),
            context_epoch: 0,
            revision: 1,
            created_by: ActorId(id("sampling-owner")),
            created_at_ms: T0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })?;
        tx.router().put_session_pin(&pin(1), None)?;
        tx.sessions().put_provider_context_head(
            &StoredProviderContextHead {
                session_id: id("agent-session"),
                context_epoch: 0,
                observed_sequence: 0,
                kind: "ProviderInitialContext".into(),
                attempt_id: None,
                binding: json!({}),
                artifact: json!(head),
                revision: 1,
            },
            None,
        )?;
        let operation = OperationId(id("mcp-tool-call"));
        let intent = tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: "mcp.tool".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload,
        })?;
        let attempt = tx.operations().begin_attempt(
            &operation,
            intent.revision,
            AttemptBinding::default(),
        )?;
        let preflighted = tx.operations().operation(&operation)?.unwrap();
        tx.operations().transition(
            &operation,
            preflighted.revision,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )?;
        Ok((binding.revision, attempt.id.0))
    })
    .unwrap();
    SamplingGrant {
        context: agent,
        operation_id: OperationId(id("mcp-tool-call")),
        attempt_id: attempt,
        call: key,
        fences: vec![AuthorityFence::Principal {
            actor: ActorId(id("sampling-agent")),
            revision,
        }],
    }
}

fn requests(f: &Fx) -> usize {
    f.http.lock().unwrap().requests.lock().unwrap().len()
}

#[test]
fn sampling_runs_in_the_agent_session_as_mcp_usage_without_transcript_or_head_change() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        let grant = agent_call(&f);
        let router = Arc::new(InferenceRouter::new(
            f.h.ids.clone(),
            f.h.clock(),
            f.connections.clone(),
        ));
        let sampling = McpSampling::new(
            f.store.clone(),
            f.artifacts.clone(),
            f.driver.clone(),
            router,
        );
        let head = |f: &Fx| {
            read(f.store.as_ref(), |s| {
                s.sessions().provider_context_head(&id("agent-session"))
            })
            .unwrap()
        };
        let before = head(&f);

        let sampled = sampling
            .sample(&grant, &call(1, "Summarize the change"), None)
            .await
            .expect("routed sample");
        assert_eq!(sampled.text, "Sampled answer");
        assert_eq!(sampled.model, "model");
        assert!(!sampled.truncated);
        assert_eq!(requests(&f), 1);
        let body = String::from_utf8(
            f.http.lock().unwrap().requests.lock().unwrap()[0]
                .body
                .clone()
                .unwrap(),
        )
        .unwrap();
        assert!(body.contains("Summarize the change") && body.contains("Answer in one line."));
        assert!(body.contains("You have no tools") && !body.contains("\"tools\""));
        // This model's registration does not qualify sampling hints, so they are omitted.
        assert!(!body.contains("temperature") && !body.contains("END"));

        // Attributed Mcp usage in the agent's session, with no transcript entry or head move.
        let request = McpSampling::request_id(&grant, 1);
        let (purpose, category, attempt) = read(f.store.as_ref(), |s| {
            let route = s.router().route_request(&request)?.unwrap();
            let operation = s.operations().operation(&route.operation_id)?.unwrap();
            let attempt = s
                .usage()
                .inference_attempt(&operation.attempts[0].id.0)?
                .unwrap();
            Ok((
                route.purpose,
                operation.payload["reportCategory"].clone(),
                attempt,
            ))
        });
        assert_eq!(purpose, "Enrichment");
        assert_eq!(category, "Mcp");
        assert_eq!(attempt.session_id, Some(id("agent-session")));
        assert_eq!(attempt.actor_id, ActorId(id("sampling-agent")));
        assert_eq!(attempt.state, "Settled");
        assert!(
            read(f.store.as_ref(), |s| s.sessions().entries(
                &id("agent-session"),
                0,
                500
            ))
            .is_empty()
        );
        assert_eq!(head(&f).artifact, before.artifact);
        assert_eq!(head(&f).revision, before.revision);

        // A replay of the same sample returns the saved text and never sends again; different
        // material under the same identity is refused.
        let replay = sampling
            .sample(&grant, &call(1, "Summarize the change"), None)
            .await
            .unwrap();
        assert_eq!(replay.text, "Sampled answer");
        assert!(
            sampling
                .sample(&grant, &call(1, "Something else"), None)
                .await
                .is_err()
        );
        assert_eq!(requests(&f), 1);

        // A pin change after planning is refused at admission, before transport.
        let planned = sampling.plan(&grant, &call(2, "Second")).unwrap();
        write(f.store.as_ref(), |tx| {
            tx.router().put_session_pin(&pin(2), Some(1))?;
            Ok(())
        })
        .unwrap();
        let refused = f.driver.produce_cancellable(planned, None).await;
        assert!(!matches!(refused, Ok(p) if p.result.outcome == Some(AttemptEnd::Completed)));
        assert_eq!(requests(&f), 1);

        // Out-of-headroom output and a settled tool call are refused without transport.
        let mut large = call(3, "Large");
        large.max_tokens = 4096;
        assert!(matches!(
            sampling.sample(&grant, &large, None).await,
            Err(SamplingRefusal::Failed(_))
        ));
        write(f.store.as_ref(), |tx| {
            let operation = OperationId(id("mcp-tool-call"));
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
        assert!(
            sampling
                .sample(&grant, &call(4, "After the call"), None)
                .await
                .is_err()
        );
        assert_eq!(requests(&f), 1);
        assert!(
            read(f.store.as_ref(), |s| s.sessions().entries(
                &id("agent-session"),
                0,
                500
            ))
            .is_empty()
        );
    });
}

#[test]
fn sampling_with_changed_authority_or_an_unroutable_pin_is_refused_before_transport() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse(WIRE), false);
        let grant = agent_call(&f);
        let router = Arc::new(InferenceRouter::new(
            f.h.ids.clone(),
            f.h.clock(),
            f.connections.clone(),
        ));
        let sampling = McpSampling::new(
            f.store.clone(),
            f.artifacts.clone(),
            f.driver.clone(),
            router,
        );
        let stale = SamplingGrant {
            fences: vec![AuthorityFence::Principal {
                actor: ActorId(id("sampling-agent")),
                revision: 99,
            }],
            context: grant.context.clone(),
            operation_id: grant.operation_id.clone(),
            attempt_id: grant.attempt_id.clone(),
            call: grant.call.clone(),
        };
        assert!(
            sampling
                .sample(&stale, &call(1, "Stale"), None)
                .await
                .is_err()
        );
        let foreign = SamplingGrant {
            call: ToolCallKey {
                native_call_id: "other-call".into(),
                ..grant.call.clone()
            },
            context: grant.context.clone(),
            operation_id: grant.operation_id.clone(),
            attempt_id: grant.attempt_id.clone(),
            fences: grant.fences.clone(),
        };
        assert!(
            sampling
                .sample(&foreign, &call(2, "Foreign"), None)
                .await
                .is_err()
        );
        write(f.store.as_ref(), |tx| {
            let mut model = tx.router().route_model(&id("model"))?.unwrap();
            let expected = model.revision;
            model.enabled = false;
            model.revision += 1;
            tx.router().put_route_model(&model, Some(expected))?;
            Ok(())
        })
        .unwrap();
        assert!(matches!(
            sampling.sample(&grant, &call(3, "Unrouted"), None).await,
            Err(SamplingRefusal::Failed(_))
        ));
        assert_eq!(requests(&f), 0);
    });
}
