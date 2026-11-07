//! Bounded failure-only Help journal/queue metadata; never guide or document bodies.
use devworks_core::ports::storage::{ApplicationStore, ReadOptions, read_snapshot};
use devworks_core::protocol::Id;
use devworks_engine::Dispatcher;
use serde_json::{Value, json};

pub(super) struct Startup<'a> {
    store: &'a dyn ApplicationStore,
    dispatcher: &'a Dispatcher,
}
pub(super) fn watch<'a>(
    store: &'a dyn ApplicationStore,
    dispatcher: &'a Dispatcher,
) -> Startup<'a> {
    Startup { store, dispatcher }
}
fn observation_reason(value: &Value) -> Option<Value> {
    let text = value
        .get("reason")
        .or_else(|| value.get("message"))?
        .as_str()?;
    let mut characters = text.chars();
    let preview: String = characters.by_ref().take(512).collect();
    Some(json!({"text":preview,"truncated":characters.next().is_some()}))
}
impl Drop for Startup<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let snapshot = read_snapshot(self.store, &ReadOptions::default(), |read| {
            let head = read.help().help_head()?;
            let operation = head
                .generation_operation_id
                .as_ref()
                .map(|id| read.operations().operation(id))
                .transpose()?
                .flatten();
            let drafts = read.help().help_drafts(None, 32)?;
            let unresolved = read.operations().unresolved(32)?;
            let delivery = operation
                .as_ref()
                .and_then(|op| op.payload["deliveryId"].as_str())
                .and_then(|id| Id::new(id).ok())
                .map(|id| read.queue().entry(&id))
                .transpose()?
                .flatten();
            let staging = unresolved.iter().filter(|op| op.capability == "artifacts.stage_file"
                && operation.as_ref().is_some_and(|owner| op.payload["source"]["generationId"] == owner.payload["generation"]))
                .map(|op| json!({"id":op.id,"state":format!("{:?}",op.state),"topic":op.payload["source"]["topicId"],"bytes":op.payload["byteCount"]})).collect::<Vec<_>>();
            Ok(json!({
                "sampledDuringFailureCleanup":true,
                "headRevision":head.revision,"publishedGeneration":head.generation_id,
                "registryRevision":head.registry_revision,"currentRegistryRevision":read.packages().registry_revision()?,
                "operation":operation.as_ref().map(|op|json!({"id":op.id,"state":format!("{:?}",op.state),"revision":op.revision,
                    "generation":op.payload["generation"],"inputDigest":op.payload["inputDigest"],
                    "latestAttempt":op.latest_attempt().map(|attempt|json!({"id":attempt.id,"bootId":attempt.boot_id,"number":attempt.attempt_no,"state":format!("{:?}",attempt.state),"observationCount":attempt.observations.len(),"latestReasons":attempt.observations.iter().rev().take(2).filter_map(observation_reason).collect::<Vec<_>>()}))})),
                "delivery":delivery.map(|row|json!({"id":row.id,"operation":row.operation_id,"state":format!("{:?}",row.state),"owner":row.owner,"generation":row.generation,"leaseUntilMs":row.lease_until_ms,"cancelRequested":row.cancel_requested})),
                "draftFirstPage":drafts.iter().map(|draft|json!({"generation":draft.generation_id,"operation":draft.operation_id})).collect::<Vec<_>>(),
                "draftPageFull":drafts.len()==32,"unresolvedPageFull":unresolved.len()==32,"stagingFromFirstPage":staging
            }))
        });
        eprintln!(
            "HELP_STARTUP_FAILURE inFlight={} dispatcherPasses={} snapshot={snapshot:?}",
            self.dispatcher.in_flight(),
            self.dispatcher.passes()
        );
    }
}
