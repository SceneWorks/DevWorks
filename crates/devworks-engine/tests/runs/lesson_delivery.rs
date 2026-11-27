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

/// Runs `op` as the person at its newest schema revision; returns the applied result value.
fn command(fx: &Fx, app: &Runtime, op: &str, mut body: Value) -> Value {
    body["kind"] = json!(op);
    let revision = devworks_core::registry::find(op)
        .and_then(|d| d.revisions.iter().map(|r| r.0).max())
        .unwrap_or(1);
    let frame = serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1",
        "schemaRevision":revision,"requestKey":fx.key(),"body":body}))
    .unwrap();
    match app.command(&fx.user, op, &frame).unwrap() {
        DomainCommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Records a lesson the person states in `project` under `applicability`; returns its id.
fn record_lesson(fx: &Fx, app: &Runtime, project: &str, text: &str, applicability: Value) -> Id {
    let value = command(
        fx,
        app,
        "knowledge.record",
        json!({
        "scope":{"kind":"Project","projectId":project},"batch":{
        "entities":[{"clientId":"s","label":"Deploys","entityType":"topic","aliases":[]}],
        "evidence":[],
        "claims":[{"kind":"Lesson","subjectId":"s","predicate":"lesson","value":text,
            "confidence":0.8,"evidenceIds":[],"tags":[],"lesson":{"source":"Prompt",
            "applicability":applicability}}]}}),
    );
    id(value["claimIds"][0].as_str().unwrap())
}

/// Applicability restricted to `tools` and `harness` (empty and `None`: unrestricted).
fn applying(tools: &[&str], harness: Option<&str>) -> Value {
    let mut a = json!({"activities":[],"paths":[],"tools":tools,"repositoryIds":[]});
    if let Some(h) = harness {
        a["harness"] = json!(h);
    }
    a
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
        let rule = record_lesson(
            &fx,
            &app,
            &project,
            "Never force-push a shared branch.",
            applying(&[], None),
        );
        assert_eq!(delivered_sessions(&*fx.store, &rule), 0);
        // Rules for a tool the session is offered apply; rules for another harness or for a
        // tool it is not offered are never bound.
        let tool = app
            .providers()
            .unwrap()
            .schemas()
            .selected_agent_tools(&context)
            .unwrap()
            .remove(0)
            .wire_name;
        let offered = record_lesson(
            &fx,
            &app,
            &project,
            "Read the session first.",
            applying(&[tool.as_str()], None),
        );
        let elsewhere = record_lesson(
            &fx,
            &app,
            &project,
            "Use the CLI tool.",
            applying(&[], Some("claude-code")),
        );
        let unoffered = record_lesson(
            &fx,
            &app,
            &project,
            "Mind the shell.",
            applying(&["no_such_tool"], None),
        );

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
        let bound_rules: Vec<&Id> = stamp.rules.iter().map(|r| &r.claim_id).collect();
        assert_eq!(bound_rules.len(), 2, "{bound_rules:?}");
        assert!(bound_rules.contains(&&rule) && bound_rules.contains(&&offered));
        assert!(!stamp.truncated);
        assert_eq!(delivered_sessions(&*fx.store, &elsewhere), 0);
        assert_eq!(delivered_sessions(&*fx.store, &unoffered), 0);

        // A rule recorded while the session runs is adopted at its next safe step, under a new
        // context epoch, and its delivery is recorded in that epoch's transaction.
        let later = record_lesson(
            &fx,
            &app,
            &project,
            "Run the migrations check first.",
            applying(&[], None),
        );
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

        // A provider change reuses the bound rules without recording their delivery again, so a
        // rule deleted since cannot refuse it; the next adoption drops the rule.
        let (head, binding, _) = app
            .recover_native_context_head(&context, &session)
            .unwrap()
            .unwrap();
        let revision = read(&*fx.store, |s| s.knowledge().knowledge_claim(&later))
            .unwrap()
            .revision;
        command(
            &fx,
            &app,
            "knowledge.delete_selected",
            json!({
            "scope":{"kind":"Project","projectId":project},
            "claims":[{"id":later,"revision":revision.to_string()}]}),
        );
        let next = devworks_core::protocol::provider::NativeBinding {
            model_id: id("smaller-model"),
            context_epoch: devworks_core::protocol::Counter::new(binding.context_epoch.get() + 1)
                .unwrap(),
            ..binding.clone()
        };
        let changed = app
            .change_provider_epoch(&context, &session, &head, &binding, &next)
            .expect("provider change with a deleted rule bound");
        assert_eq!(
            context_at(&app, &context, &changed).lesson_stamp,
            context_at(&app, &context, &head).lesson_stamp
        );
        assert_eq!(delivered_sessions(&*fx.store, &later), 1);
        app.shutdown().await;
    });
}
