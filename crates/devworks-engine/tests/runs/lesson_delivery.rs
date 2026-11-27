//! Effective lesson rules reach an agent's provider context as harness instructions, a rule
//! recorded later reaches the running session at its next safe step, and every delivery is
//! recorded for the session (KNOW-06). Provider bytes are scripted; no model runs.
use super::approval_wait::{Body, pinned_agent_session, runtime_with};
use super::*;
use devworks_core::ports::provider_http::{
    HttpStream, StreamLimits, StreamingHttp, TransportError,
};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::providers::context::ProviderContext;
use devworks_core::providers::instructions::LESSON_BLOCK_HEADER;
use devworks_engine::Runtime;

/// Answers every request with a completed answer and keeps each request body.
#[derive(Default)]
struct Recording(Mutex<Vec<Vec<u8>>>);
impl StreamingHttp for Recording {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.0
            .lock()
            .unwrap()
            .push(request.body.clone().unwrap_or_default());
        Ok(Box::new(Body(Some(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n\
              data: [DONE]\n\n"
                .to_vec(),
        ))))
    }
}

/// Records a lesson the person states in `project`; returns its claim id.
fn record_lesson(fx: &Fx, app: &Runtime, project: &str, text: &str) -> Id {
    let frame = serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1",
        "schemaRevision":2,"requestKey":fx.key(),"body":{"kind":"knowledge.record",
        "scope":{"kind":"Project","projectId":project},"batch":{
        "entities":[{"clientId":"s","label":"Deploys","entityType":"topic","aliases":[]}],
        "evidence":[],
        "claims":[{"kind":"Lesson","subjectId":"s","predicate":"lesson","value":text,
            "confidence":0.8,"evidenceIds":[],"tags":[],"lesson":{"source":"Prompt",
            "applicability":{"activities":[],"paths":[],"tools":[],"repositoryIds":[]}}}]}}}))
    .unwrap();
    match app.command(&fx.user, "knowledge.record", &frame).unwrap() {
        DomainCommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => {
                let value = serde_json::to_value(result).unwrap();
                id(value["value"]["claimIds"][0].as_str().unwrap())
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// The sessions a rule was recorded as supplied to.
fn delivered_sessions(store: &dyn ApplicationStore, rule: &Id) -> i64 {
    read(store, |s| s.knowledge().knowledge_deliveries(rule)).sessions
}

/// The provider context a published head holds.
fn context_at(app: &Runtime, ctx: &InvocationContext, head: &ArtifactRef) -> ProviderContext {
    let bytes = app
        .artifacts()
        .read_exact(app.store().as_ref(), ctx, head, 8 * 1024 * 1024)
        .unwrap()
        .0;
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn an_effective_lesson_reaches_the_provider_context_and_its_delivery_is_recorded() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let wire = Arc::new(Recording::default());
        let app = runtime_with(&fx, Arc::new(Authority), wire.clone(), |_| {}).await;
        let (context, agent) = pinned_agent_session(&fx);
        let session = context.session_id().unwrap().clone();
        let project = context.project_id().unwrap().0.as_str().to_owned();
        let rule = record_lesson(&fx, &app, &project, "Never force-push a shared branch.");
        assert_eq!(delivered_sessions(&*fx.store, &rule), 0);

        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let end = NativeAgentTurnExecutor::new(&app)
            .run(context.clone(), agent.clone(), stop)
            .await;
        assert!(
            matches!(
                end,
                LaunchEnd::Finished {
                    outcome: ActivationOutcome::Completed,
                    ..
                }
            ),
            "{end:?}"
        );

        // The provider received the rule as harness instructions, never as a user message.
        let bodies = wire.0.lock().unwrap().clone();
        assert!(!bodies.is_empty(), "the turn sent no provider request");
        let request: Value = serde_json::from_slice(&bodies[0]).unwrap();
        let messages = request["messages"].as_array().unwrap();
        let carrying: Vec<&Value> = messages
            .iter()
            .filter(|m| m.to_string().contains("Never force-push a shared branch."))
            .collect();
        assert_eq!(carrying.len(), 1, "{messages:?}");
        assert_eq!(carrying[0]["role"], "system", "{messages:?}");
        let header = serde_json::to_string(LESSON_BLOCK_HEADER.trim_end()).unwrap();
        assert!(
            carrying[0].to_string().contains(header.trim_matches('"')),
            "{carrying:?}"
        );
        // One delivery is recorded for the session, with the revision the context carries.
        assert_eq!(delivered_sessions(&*fx.store, &rule), 1);
        let (head, binding, _) = app
            .recover_native_context_head(&context, &session)
            .unwrap()
            .unwrap();
        let bound = context_at(&app, &context, &head);
        let stamp = bound.lesson_stamp.expect("lesson stamp");
        assert_eq!(stamp.rules.len(), 1);
        assert_eq!(stamp.rules[0].claim_id, rule);

        // A rule recorded while the session runs is adopted at its next safe step, under a new
        // context epoch, and its delivery is recorded in that epoch's transaction.
        let later = record_lesson(&fx, &app, &project, "Run the migrations check first.");
        let adopted = app
            .adopt_instruction_epoch(&context, &session, &head, &binding, &[])
            .unwrap();
        assert_ne!(adopted.version_id, head.version_id);
        let rules: Vec<Id> = context_at(&app, &context, &adopted)
            .lesson_stamp
            .unwrap()
            .rules
            .into_iter()
            .map(|r| r.claim_id)
            .collect();
        assert!(rules.contains(&rule) && rules.contains(&later), "{rules:?}");
        assert_eq!(delivered_sessions(&*fx.store, &later), 1);
        assert_eq!(
            delivered_sessions(&*fx.store, &rule),
            1,
            "not recorded twice"
        );
        app.shutdown().await;
    });
}
