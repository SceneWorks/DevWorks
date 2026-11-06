use super::*;

#[test]
fn aggregate_native_text_order_is_retained_by_the_actual_session_writer() {
    runtime().block_on(async {
        for (profile, body) in [
            ("AnthropicMessages", json!({"model":"fixture-model","content":[{"type":"thinking","thinking":"first","signature":"actual-signature"},{"type":"text","text":"middle"},{"type":"redacted_thinking","data":"actual-opaque"},{"type":"text","text":"last"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})),
            ("OpenAiResponses", json!({"model":"fixture-model","status":"completed","output":[{"id":"r1","type":"reasoning","encrypted_content":"actual-first","summary":[]},{"id":"m1","type":"message","content":[{"type":"output_text","text":"middle"}]},{"id":"r2","type":"reasoning","encrypted_content":"actual-last","summary":[]},{"id":"m2","type":"message","content":[{"type":"output_text","text":"last"}]}],"usage":{"input_tokens":1,"output_tokens":1}})),
        ] {
            let f = Fx::new(profile, Script::aggregate(&body.to_string()), false);
            let result = f.produce(f.generation(vec![])).await.unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::Completed));
            let order: Vec<_> = f.transcript().into_iter().map(|entry| entry.kind).collect();
            assert_eq!(order, ["NativeItem", "AssistantAttempt", "NativeItem", "AssistantAttempt"], "{profile}");
        }
    });
}
