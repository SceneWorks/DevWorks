//! Runs a package's event handler when a host event it declares fires.
//!
//! # Which events
//!
//! The host publishes one package event today: `PackageAvailabilityChanged`, announced by the
//! package service in the unit of work that changed a package's availability or active version
//! (schema [`AVAILABILITY_SCHEMA`]). A package declares interest with an `EventHandler`
//! contribution whose `eventSchemas` name it.
//!
//! # Delivery
//!
//! Two consumers, both durable queue consumers:
//!
//! * [`ROUTER`] receives the announcement. In the same transaction that settles it, it writes
//!   one delivery event and one queue entry on [`CONSUMER`] for every `EventHandler`
//!   contribution of an installed, enabled package's active version. An announcement is
//!   therefore fanned out exactly once, however often the router's transaction is retried.
//! * [`CONSUMER`] runs one handler per entry as an asynchronous handler: it checks the handler
//!   still applies, stages the event as the invocation's input artifact, and starts an
//!   [`InvocationKind::Event`] invocation whose commit (`broker.commit_event`) settles the
//!   entry together with the handler's state, effect intents and processing receipt.
//!
//! # Cascades
//!
//! A handler whose effect intent changes a package announces a new event. That announcement
//! carries the originating package and a cascade depth ([`MAX_CASCADE_DEPTH`]). The router
//! delivers it as a skip (logged in the package's log) to the originating package's own
//! handlers, and to every handler once the depth passes the bound.
//!
//! # Reruns
//!
//! A delivery recovered after a crash runs the handler again only when it has no processing
//! receipt; one with a receipt is settled without running. The invocation keeps one operation
//! identity per delivery, so a rerun's brokered calls are the same calls: its inference carries
//! the same operation and action key, so a recorded answer is replayed and never charged twice.
//! The handler's input artifact is one per delivery (see `stage_input`). A rerun's brokered
//! inference publishes its answer again as an artifact of the approving person; those answers
//! are ordinary explicitly retained artifacts that artifact collection does not reclaim.
//!
//! # Who a handler runs as
//!
//! An event handler has no invoking principal. It runs as the person who approved that exact
//! version ([`super::intents::approving_person`]), narrowed by the version's approved manifest
//! grants and that person's current authority on every brokered call. A version no person
//! approved (activated under publisher trust, or plainly) runs no handler: its deliveries are
//! settled without an invocation and the package's log says so.
use super::{InvocationKind, InvokeOutcome, InvokeRefusal, InvokeRequest, PackageHost, recovery};
use crate::dispatch::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
    DeliveryHandler, Outcome, RetryPolicy,
};
use crate::package::{PackageInvocation, expected_checkpoint};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::{
    ApplicationStore, ArtifactScope, EventFilter, NewEvent, NewQueueEntry, ReadOptions,
    StorageError, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use devworks_core::protocol::provider::ArtifactRef as HostArtifact;
use devworks_core::protocol::records::Severity;
use devworks_core::protocol::{EventId, Id};
use devworks_core::services::artifacts::{host::HostArtifactPublication, version_ref};
use devworks_core::services::packages::index::AVAILABILITY_CHANGED;
use devworks_core::services::packages::{policy, view};
use devworks_extension_protocol::package::{InvocationOutcome, InvocationScope};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

/// The consumer that fans an announcement out to the handlers that declare it.
pub const ROUTER: &str = "package-event-router";
/// The consumer that runs one handler per delivery.
pub const CONSUMER: &str = "package-events";
/// The schema of `PackageAvailabilityChanged` an `EventHandler` contribution names.
pub const AVAILABILITY_SCHEMA: &str = "devworks/package-availability-changed";
/// How long one event handler may run before the host cancels it.
pub const DEADLINE: Duration = Duration::from_secs(5 * 60);
/// The history event kind of one handler's delivery.
const DELIVERED: &str = "PackageEventDelivered";
/// The deepest cascade of handler-caused events that still reaches handlers: an announcement
/// caused by an effect intent of a handler that an announcement caused (and so on) is one deeper
/// than that handler's delivery, and one beyond this depth is not dispatched.
pub const MAX_CASCADE_DEPTH: u64 = 3;

/// The host event schema `kind` is announced under, when it is a package event.
fn schema_of(kind: &str) -> Option<&'static str> {
    (kind == AVAILABILITY_CHANGED).then_some(AVAILABILITY_SCHEMA)
}

/// The operation identity of the invocation of delivery `delivery`: one per delivery, so a
/// rerun's brokered calls keep their identity.
fn operation_of(delivery: &Id) -> Id {
    Id::new(format!(
        "pkgevt-{}",
        sha256_hex(delivery.as_str().as_bytes())
    ))
    .expect("bounded digest ID")
}

struct Router;
impl DeliveryHandler for Router {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        let Some(schema) = schema_of(&delivery.event.kind) else {
            return Ok(Outcome::Completed);
        };
        let registry = view::load(&*tx.packages())?;
        let actor = tx.store().host_actor.clone();
        // Who caused the announcement: a package's handler's effect intent, at some depth.
        let origin = delivery.entry.payload["originPackageId"].as_str();
        let depth = delivery.entry.payload["depth"].as_u64().unwrap_or(0);
        for (package, (record, active)) in &registry {
            let Some(version) = active
                .as_ref()
                .filter(|_| record.installed && record.enabled)
            else {
                continue;
            };
            let Some(inspection) = policy::inspection(version) else {
                continue;
            };
            for handler in inspection
                .contributions
                .iter()
                .filter(|c| c.kind == "EventHandler")
            {
                // A handler never hears of what its own intent caused, and a cascade of
                // handler-caused events stops at the depth bound. The delivery is still written
                // (so the package's log says why) and settles without running the handler.
                let skip = if origin == Some(package.as_str()) {
                    Some("its own effect intent caused the event")
                } else if depth > MAX_CASCADE_DEPTH {
                    Some("the event is too deep in a cascade of events handlers caused")
                } else {
                    None
                };
                let event = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent {
                    id: event.clone(),
                    actor: actor.clone(),
                    project_id: None,
                    root_id: None,
                    operation_id: None,
                    aggregate_kind: "package_event".into(),
                    aggregate_id: package.clone(),
                    aggregate_revision: 1,
                    kind: DELIVERED.into(),
                    payload: json!({
                        "schemaId": schema, "contributionId": handler.id,
                        "digest": version.manifest_digest,
                        "sourceEventId": delivery.event.id, "sourceKind": delivery.event.kind,
                        "event": delivery.entry.payload, "depth": depth, "skip": skip,
                    }),
                })?;
                let entry = tx.new_id(IdKind::QueueEntry);
                let ready_at_ms = tx.now_ms();
                tx.queue().enqueue(NewQueueEntry {
                    id: entry,
                    consumer: CONSUMER.into(),
                    event_id: event,
                    root_id: None,
                    operation_id: None,
                    partition: None,
                    priority: 0,
                    ready_at_ms,
                    payload: json!({"packageId": package, "contributionId": handler.id,
                        "digest": version.manifest_digest, "schemaId": schema,
                        "depth": depth, "skip": skip}),
                })?;
            }
        }
        Ok(Outcome::Completed)
    }
}

/// The consumer that fans package announcements out to their handlers.
pub fn router() -> Consumer {
    Consumer {
        name: ROUTER.into(),
        handler: Arc::new(Router),
        effect: None,
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy::default(),
        max_in_flight: None,
    }
}

struct Handlers {
    store: Arc<dyn ApplicationStore>,
    host: Arc<OnceLock<Weak<PackageHost>>>,
}

struct Settle;
impl DeliveryHandler for Settle {
    fn apply(
        &self,
        _tx: &mut dyn WriteTransaction,
        _delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}

/// The consumer that runs event handlers on `host` (set once it exists).
pub fn consumer(
    store: Arc<dyn ApplicationStore>,
    host: Arc<OnceLock<Weak<PackageHost>>>,
) -> Consumer {
    Consumer {
        name: CONSUMER.into(),
        handler: Arc::new(Settle),
        effect: None,
        async_handler: Some(Arc::new(Handlers { store, host })),
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            max_attempts: 4,
        },
        max_in_flight: Some(2),
    }
}

impl AsyncDeliveryHandler for Handlers {
    fn start(&self, delivery: AsyncDelivery) -> AsyncFuture {
        let (store, host) = (self.store.clone(), self.host.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || run(&store, &host, delivery))
                .await
                .unwrap_or_else(|_| {
                    AsyncOutcome::Hold("the event handler delivery panicked".into())
                })
        })
    }
}

/// What the delivery names (the router wrote it).
struct Target {
    package: String,
    contribution: String,
    digest: String,
    schema: String,
}
/// The opaque subscription id of a handler contribution. A qualified id such as
/// `pkg/on.changed` may hold periods or exceed the opaque id's 128 characters, so those get
/// `sha256-` and the digest of the whole contribution id. Every other contribution keeps its
/// `/`-to-`--` form, the id its stored processing receipts already carry; the two forms cannot
/// meet, since only the first has `--`.
fn subscription_id(contribution: &str) -> Result<Id, &'static str> {
    Id::new(contribution.replace('/', "--"))
        .or_else(|_| Id::new(format!("sha256-{}", sha256_hex(contribution.as_bytes()))))
}

fn target(payload: &Value) -> Option<Target> {
    let field = |name: &str| payload.get(name)?.as_str().map(str::to_owned);
    Some(Target {
        package: field("packageId")?,
        contribution: field("contributionId")?,
        digest: field("digest")?,
        schema: field("schemaId")?,
    })
}

/// Settles a delivery that runs no handler, and says why in the package's log.
fn skipped(host: &PackageHost, package: &str, why: &str) -> AsyncOutcome {
    host.service.record_log(
        package,
        Severity::Info,
        "PackageEventSkipped",
        &format!("An event was not delivered to the package's handler: {why}"),
        None,
    );
    AsyncOutcome::Committed
}

fn run(
    store: &Arc<dyn ApplicationStore>,
    host: &Arc<OnceLock<Weak<PackageHost>>>,
    delivery: AsyncDelivery,
) -> AsyncOutcome {
    let Some(host) = host.get().and_then(Weak::upgrade) else {
        return AsyncOutcome::Retry("the package host is not running".into());
    };
    let Some(ports) = host.broker.get() else {
        return AsyncOutcome::Retry("package artifacts are not composed".into());
    };
    let AsyncDelivery {
        delivery: Delivery { entry, event },
        cancelled,
    } = delivery;
    let Some(target) = target(&entry.payload) else {
        return AsyncOutcome::Poison("invalid package event delivery".into());
    };
    if let Some(why) = entry.payload["skip"].as_str() {
        return skipped(&host, &target.package, why);
    }
    let Ok(subscription) = subscription_id(&target.contribution) else {
        return AsyncOutcome::Poison("invalid handler identity".into());
    };
    let checked = read_snapshot(&**store, &ReadOptions::default(), |s| {
        if s.packages()
            .processing(&target.package, &subscription, &event.id)?
            .is_some()
        {
            return Ok(Err(None));
        }
        let approved = super::intents::approving_person(s, &target.package, &target.digest)?;
        let Ok((_, version)) = approved else {
            return Ok(Err(Some(match approved {
                Err(super::intents::Refusal::NoApprover) => {
                    "no person approved the running version, so it runs no event handlers"
                }
                _ => "the package or the version that was announced is no longer available",
            })));
        };
        Ok(Ok((version, expected_checkpoint(s.queue(), &entry)?)))
    });
    let (version, checkpoint) = match checked {
        Ok(Ok(found)) => found,
        // Already processed: the commit that did it settled the delivery.
        Ok(Err(None)) => return AsyncOutcome::Committed,
        Ok(Err(Some(why))) => return skipped(&host, &target.package, why),
        Err(e) => return AsyncOutcome::Retry(format!("{e:?}")),
    };
    let Some(mut inspection) = policy::inspection(&version) else {
        return AsyncOutcome::Poison("the version's inspection is invalid".into());
    };
    if recovery::compact(&inspection) {
        match recovery::recover(
            &**store,
            &ports.artifacts,
            &host.service,
            &host.host_ctx(),
            &version,
        ) {
            Ok(full) => inspection = full,
            Err(_) => {
                return AsyncOutcome::Hold("the version's handlers cannot be verified".into());
            }
        }
    }
    let declared = inspection
        .contributions
        .iter()
        .find(|c| c.id == target.contribution)
        .and_then(|c| c.verified_descriptor())
        .filter(|d| d["kind"] == "EventHandler")
        .is_some_and(|d| {
            d["eventSchemas"]
                .as_array()
                .is_some_and(|all| all.iter().any(|s| s["schemaId"] == target.schema))
        });
    if !declared {
        // The version has a handler, but not for this event: nothing to log.
        return AsyncOutcome::Committed;
    }
    let document = json!({
        "schemaId": target.schema, "subscriptionId": subscription, "eventId": event.id,
        "contributionId": target.contribution, "expectedCheckpoint": checkpoint.to_string(),
        "event": event.payload,
    });
    let input = match stage_input(&host, ports, &document, &entry.id) {
        Ok(input) => input,
        Err(why) => return AsyncOutcome::Retry(why),
    };
    let invocation = PackageInvocation {
        package_id: target.package.clone(),
        subscription_id: subscription.clone(),
        process_session_id: String::new(),
        invocation_id: String::new(),
        invocation_token: String::new(),
        state_generation: 0,
        delivery_id: entry.id.clone(),
        fence: entry.fence().expect("a delivered entry is claimed"),
        cancellation_epoch: entry.cancellation_epoch,
        actor: store.info().host_actor,
        broker_consumer: super::intents::CONSUMER.into(),
    };
    let admitted = host.invoke(InvokeRequest {
        package_id: target.package.clone(),
        contribution_id: target.contribution.clone(),
        scope: InvocationScope::Workspace,
        input,
        operation_id: operation_of(&entry.id),
        deadline: DEADLINE,
        allowed_operations: Vec::new(),
        kind: InvocationKind::Event(Box::new(invocation)),
        principal: None,
        expected_digest: Some(target.digest.clone()),
    });
    let handle = match admitted {
        Ok(handle) => handle,
        // Nothing reached the package: try again under the consumer's policy.
        Err(
            refusal @ (InvokeRefusal::NotReady(_)
            | InvokeRefusal::ShuttingDown
            | InvokeRefusal::Storage(_)),
        ) => return AsyncOutcome::Retry(format!("{refusal:?}")),
        Err(refusal) => return skipped(&host, &target.package, &format!("{refusal:?}")),
    };
    let mut asked = false;
    let outcome = loop {
        if let Some(outcome) = handle.wait(Duration::from_millis(200)) {
            break outcome;
        }
        if !asked && *cancelled.borrow() {
            asked = true;
            break handle.cancel();
        }
    };
    let receipt = || {
        read_snapshot(&**store, &ReadOptions::default(), |s| {
            Ok(s.packages()
                .processing(&target.package, &subscription, &event.id)?
                .is_some())
        })
        .unwrap_or(false)
    };
    match outcome {
        InvokeOutcome::Finished(InvocationOutcome::Completed { .. }) if receipt() => {
            AsyncOutcome::Committed
        }
        InvokeOutcome::Finished(InvocationOutcome::Completed { .. }) => {
            AsyncOutcome::Poison("the handler finished without committing its event".into())
        }
        _ if asked => AsyncOutcome::Retry("the delivery was stopped".into()),
        InvokeOutcome::Finished(InvocationOutcome::Failed { code }) => {
            AsyncOutcome::Poison(format!("the handler failed: {}", code.as_str()))
        }
        InvokeOutcome::Finished(InvocationOutcome::CancelledBeforeEffect) => {
            AsyncOutcome::Retry("the handler was cancelled before any effect".into())
        }
        InvokeOutcome::Finished(InvocationOutcome::OutcomeUnknown { .. }) => {
            AsyncOutcome::Hold("the handler reports its outcome unknown".into())
        }
        InvokeOutcome::Failed(why) => AsyncOutcome::Poison(why),
        InvokeOutcome::Unknown(why) => AsyncOutcome::Hold(why),
    }
}

impl PackageHost {
    /// The handler input artifact of `delivery` holding `document`: the one an earlier attempt
    /// recorded, or a new one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn stage_event_input(
        &self,
        document: &Value,
        delivery: &Id,
    ) -> Result<wire::ArtifactRef, String> {
        let ports = self
            .broker
            .get()
            .ok_or("package artifacts are not composed")?;
        stage_input(self, ports, document, delivery)
    }
}

/// The history aggregate recording the input artifact of a delivery.
const INPUT_AGGREGATE: &str = "package_event_input";

/// Saves `document` as a host artifact: the handler's input. It is one per delivery: a rerun of
/// the delivery (after a crash or a retry) finds the artifact the first attempt recorded in
/// history and reuses it, so attempts never accumulate artifacts. The artifact is kept after the
/// delivery settles, as the record of what the handler was given (explicitly retained artifacts
/// are not reclaimed by artifact collection).
fn stage_input(
    host: &PackageHost,
    ports: &super::broker::BrokerPorts,
    document: &Value,
    delivery: &Id,
) -> Result<wire::ArtifactRef, String> {
    let bytes = document.to_string().into_bytes();
    let digest = sha256_hex(&bytes);
    let aggregate = EventFilter::Aggregate {
        kind: INPUT_AGGREGATE.into(),
        id: delivery.as_str().to_owned(),
    };
    let earlier = read_snapshot(&*host.store, &ReadOptions::default(), |s| {
        for event in s.history().page(&aggregate, 0, 16)?.events {
            let Ok(reference) =
                serde_json::from_value::<HostArtifact>(event.payload["input"].clone())
            else {
                continue;
            };
            let live = s
                .artifacts()
                .version(&reference.version_id)?
                .is_some_and(|v| v.live() && v.blob_digest == digest);
            if live && String::from(reference.sha256.clone()) == digest {
                return Ok(Some(reference));
            }
        }
        Ok(None)
    })
    .map_err(|e| format!("{e:?}"))?;
    if let Some(reference) = earlier {
        return serde_json::to_value(&reference)
            .and_then(serde_json::from_value)
            .map_err(|e| e.to_string());
    }
    let staged = ports
        .artifacts
        .stage_host_bytes(
            &*host.store,
            &bytes,
            json!({"kind": "PackageEvent", "deliveryId": delivery}),
        )
        .map_err(|e| format!("{e:?}"))?
        .map_err(|e| e.to_string())?;
    let (service, pending, owner, aggregate_id) = (
        ports.artifacts.clone(),
        staged.clone(),
        host.store.info().host_actor,
        delivery.as_str().to_owned(),
    );
    let provenance = json!({"kind": "PackageEvent", "deliveryId": delivery});
    let committed = run_write(&*host.store, WriteOptions::observed(), move |tx| {
        let version = service.commit_host_bytes_in_scope(
            tx,
            &pending,
            HostArtifactPublication {
                title: "Package event",
                media_type: "application/json",
                provenance,
                scope: ArtifactScope::Workspace,
                owner: owner.clone(),
            },
        )?;
        let reference = version_ref(&version);
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event,
            actor: owner,
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: INPUT_AGGREGATE.into(),
            aggregate_id,
            aggregate_revision: 1,
            kind: "PackageEventInputStaged".into(),
            payload: json!({"input": reference}),
        })?;
        Ok(reference)
    });
    ports
        .artifacts
        .settle_host_bytes(&*host.store, &staged, committed.is_ok());
    let reference: HostArtifact = committed.map_err(|e| format!("{e:?}"))?;
    serde_json::to_value(&reference)
        .and_then(serde_json::from_value)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::subscription_id;

    #[test]
    fn a_valid_contribution_id_keeps_the_subscription_id_receipts_already_carry() {
        assert_eq!(
            subscription_id("sample-pack/on_changed").unwrap().as_str(),
            "sample-pack--on_changed"
        );
    }

    #[test]
    fn a_dotted_or_long_contribution_id_gets_a_bounded_digest_subscription_id() {
        let dotted = subscription_id("sample-pack/on.changed").unwrap();
        assert!(dotted.as_str().starts_with("sha256-"), "{dotted:?}");
        let long = format!("{}/{}", "p".repeat(64), "h".repeat(64));
        let long_id = subscription_id(&long).unwrap();
        assert!(long_id.as_str().len() <= 128);
        // Stable, and distinct per contribution.
        assert_eq!(dotted, subscription_id("sample-pack/on.changed").unwrap());
        assert_ne!(dotted, subscription_id("sample-pack/on.other").unwrap());
        assert_ne!(dotted, long_id);
    }
}
