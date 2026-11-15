//! Executes the effect intents an event handler commits with `broker.commit_event`.
//!
//! [`crate::package::commit_event_from`] records each intent as a durable `package.broker.*`
//! operation and queues it for [`CONSUMER`]; the dispatcher then runs it as an ordinary external
//! effect (transaction A marks it `Dispatching` before [`execute`] runs, transaction B records
//! the outcome). A crash between the two leaves it `Dispatching`, which recovery classifies
//! `OutcomeUnknown` and holds for reconciliation: an intent is never executed twice. Its
//! application call is framed under a request key derived from the operation, so even a
//! reconciled redelivery replays the original receipt instead of deciding again.
//!
//! # Who an intent runs as
//!
//! Event handlers have no invoking principal. An intent runs as the person who approved the
//! exact version that committed it (its version approval of kind `Person`, whose `by` must be a
//! human actor), narrowed by that version's approved manifest grants for the committing
//! contribution ([`broker::Grants`]): it never runs with more authority than either allows, and
//! the gateway admits it under that person's own current authority. An intent never runs a
//! human-only operation. A version no person approved (activated under publisher trust) runs
//! none of its intents, and neither does a package that was disabled or uninstalled since.
//!
//! The grant was checked when the intent was committed; it is checked again here against the
//! committing version, so nothing outside it ever executes. A refusal fails the operation with a
//! cause naming its [`Refusal`] code.
use super::{PackageHost, broker, recovery};
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::context::{HostIngress, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{
    ApplicationStore, Cancellation, OperationRecord, ReadOptions, StorageError, WriteTransaction,
    read_snapshot,
};
use devworks_core::protocol::provider::ArtifactRef as HostArtifact;
use devworks_core::protocol::{ActorId, Counter, Id, OperationState, Revision};
use devworks_core::services::packages::policy;
use devworks_extension_protocol::package::BrokerCall;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock, Weak};

/// The consumer that executes committed event intents; an event invocation's
/// `broker_consumer`.
pub const CONSUMER: &str = "package-intents";
/// Largest arguments artifact an intent reads.
const MAX_ARGUMENTS: u64 = 8 * 1024 * 1024;

/// Why an intent was refused without executing (the operation's failure cause starts with it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Recorded without the contribution and version that committed it.
    NoOrigin,
    /// The package was disabled or uninstalled, or the committing version is gone.
    PackageUnavailable,
    /// No person approved the committing version.
    NoApprover,
    /// The committing version's approved manifest does not grant it.
    OutsideGrant,
    /// The call kind is not executable as an intent yet.
    Unsupported,
}

fn refused(code: Refusal, why: &str) -> EffectResult {
    EffectResult::Failed(format!("{code:?}: {why}"))
}

struct Effects {
    store: Arc<dyn ApplicationStore>,
    host: Arc<OnceLock<Weak<PackageHost>>>,
}

/// What the intent's version records: the approver and the approved inspection.
struct Origin {
    approver: ActorId,
    version: devworks_core::ports::storage::StoredPackageVersion,
}

/// Runs one dispatched intent operation (blocking).
fn execute(
    store: &Arc<dyn ApplicationStore>,
    host: &Arc<OnceLock<Weak<PackageHost>>>,
    op: &OperationRecord,
) -> EffectResult {
    if op.state != OperationState::Dispatching || !op.capability.starts_with("package.broker.") {
        return EffectResult::Failed("not a dispatched package intent".into());
    }
    let p = &op.payload;
    let (Some(package), Some(call)) = (
        p["packageId"].as_str(),
        serde_json::from_value::<BrokerCall>(p["call"].clone()).ok(),
    ) else {
        return EffectResult::Failed("invalid package intent".into());
    };
    let (Some(contribution), Some(digest)) =
        (p["contributionId"].as_str(), p["manifestDigest"].as_str())
    else {
        return refused(Refusal::NoOrigin, "the intent names no committing version");
    };
    let origin = read_snapshot(&**store, &ReadOptions::default(), |s| {
        let record = s.packages().package(package)?;
        if !record.is_some_and(|r| r.installed && r.enabled) {
            return Ok(Err(Refusal::PackageUnavailable));
        }
        let Some(version) = s.packages().package_version(package, digest)? else {
            return Ok(Err(Refusal::PackageUnavailable));
        };
        let approval = version.approval.clone().unwrap_or(Value::Null);
        let approver = approval["by"]
            .as_str()
            .filter(|_| approval["kind"] == "Person")
            .and_then(|by| Id::new(by).ok())
            .map(ActorId);
        let Some(approver) = approver else {
            return Ok(Err(Refusal::NoApprover));
        };
        if s.actors().actor_kind(&approver)? != Some(PrincipalKind::Human) {
            return Ok(Err(Refusal::NoApprover));
        }
        Ok(Ok(Origin { approver, version }))
    });
    let origin = match origin {
        Ok(Ok(origin)) => origin,
        Ok(Err(code)) => return refused(code, "the intent's package or approval no longer holds"),
        Err(e) => return EffectResult::NotApplied(format!("{e:?}")),
    };
    let Some(host) = host.get().and_then(Weak::upgrade) else {
        return EffectResult::NotApplied("the package host is not running".into());
    };
    let Some(ports) = host.broker.get() else {
        return EffectResult::NotApplied("application calls are not composed".into());
    };
    let Some(gateway) = ports.gateway.upgrade() else {
        return EffectResult::NotApplied("the application stopped".into());
    };
    let info = store.info();
    let (Ok(epoch), Ok(policy_revision)) =
        (Counter::new(info.restore_epoch.max(0)), Revision::new(1))
    else {
        return EffectResult::Failed("invalid store epoch".into());
    };
    let principal = HostIngress::new(&**store).context(
        origin.approver.clone(),
        PrincipalKind::Human,
        op.id.0.clone(),
        Id::new("package-intents").expect("static ingress"),
        epoch,
        policy_revision,
    );
    let Some(mut inspection) = policy::inspection(&origin.version) else {
        return refused(Refusal::OutsideGrant, "the version's inspection is invalid");
    };
    if recovery::compact(&inspection) {
        match recovery::recover(
            &**store,
            &ports.artifacts,
            &host.service,
            &host.host_ctx(),
            &origin.version,
        ) {
            Ok(full) => inspection = full,
            Err(_) => {
                return refused(
                    Refusal::OutsideGrant,
                    "the version's grant cannot be verified",
                );
            }
        }
    }
    let grants = broker::Grants::resolve(&inspection, contribution, Some(principal), &[]);
    let BrokerCall::Application {
        operation,
        input_schema,
        arguments,
    } = &call
    else {
        return match call {
            // A read is not an effect: nothing to do.
            BrokerCall::ArtifactRead { .. } => EffectResult::Completed(json!({"effect": "None"})),
            _ => refused(Refusal::Unsupported, "this call kind is not executable yet"),
        };
    };
    let admitted = match broker::admit(&grants, operation.as_str(), input_schema) {
        Ok(a) if !a.descriptor.human_only => a,
        Ok(_) => {
            return refused(
                Refusal::OutsideGrant,
                "a package intent never runs a human-only operation",
            );
        }
        Err(e) => return refused(Refusal::OutsideGrant, &e.message),
    };
    let Ok(reference) =
        serde_json::to_value(arguments).and_then(serde_json::from_value::<HostArtifact>)
    else {
        return EffectResult::Failed("invalid arguments artifact".into());
    };
    let bytes = match ports.artifacts.read_exact(
        &*host.store,
        admitted.principal,
        &reference,
        MAX_ARGUMENTS,
    ) {
        Ok((bytes, _)) => bytes,
        Err(_) => {
            return EffectResult::Failed(
                "the arguments artifact is not readable by the approving person".into(),
            );
        }
    };
    let Ok(args) = devworks_core::framing::parse(&bytes) else {
        return EffectResult::Failed("the arguments are not a JSON object".into());
    };
    // The operation's own identity keys the request: a redelivery replays its receipt.
    let identity = sha256_hex(op.id.0.as_str().as_bytes());
    let frame = match crate::tool_turn::frame_as(
        format!("pkgintent-{identity}"),
        admitted.descriptor,
        admitted.mode,
        admitted.revision,
        args,
    ) {
        Ok(frame) => frame,
        Err(e) => return EffectResult::Failed(e.into()),
    };
    match broker::decide(
        &*gateway,
        &admitted,
        &frame,
        Cancellation::default(),
        &|decide| Some(decide()),
    ) {
        broker::Ended::Answered(state, reply) => {
            let produced = match broker::publish(
                &host,
                &ports.artifacts,
                admitted.principal,
                &op.id,
                None,
                &reply,
            ) {
                Ok(produced) => produced,
                Err(e) => {
                    return EffectResult::Unknown(format!("the reply was not published: {e:?}"));
                }
            };
            if state == OperationState::ObservedComplete {
                EffectResult::Completed(json!({"reply": produced}))
            } else {
                EffectResult::Failed(format!(
                    "the operation rejected the intent (reply {})",
                    produced.artifact_id.as_str()
                ))
            }
        }
        broker::Ended::Failed(_, OperationState::OutcomeUnknown, detail) => {
            EffectResult::Unknown(detail)
        }
        broker::Ended::Failed(_, _, detail) => EffectResult::Failed(detail),
    }
}

impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let (store, host) = (self.store.clone(), self.host.clone());
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || execute(&store, &host, &operation)).await {
                Ok(result) => result,
                Err(_) => EffectResult::Unknown("the package intent panicked".into()),
            }
        })
    }

    fn observe(&self, _operation: OperationRecord) -> EffectFuture {
        // An intent's effect is not read back after the fact: a person reconciles it.
        Box::pin(async {
            EffectResult::Unknown("a package intent's outcome cannot be observed".into())
        })
    }
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

/// The consumer that executes committed event intents on `host` (set once it exists).
pub fn consumer(
    store: Arc<dyn ApplicationStore>,
    host: Arc<OnceLock<Weak<PackageHost>>>,
) -> Consumer {
    Consumer {
        name: CONSUMER.into(),
        handler: Arc::new(Settle),
        effect: Some(Arc::new(Effects { store, host })),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            max_attempts: 4,
        },
        max_in_flight: None,
    }
}
