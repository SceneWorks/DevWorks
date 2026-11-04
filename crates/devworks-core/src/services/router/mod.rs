//! `InferenceRouter`: direct tiers, named models, Open Router Auto, effort, availability and
//! durable session pins behind `router.catalog`, `.route`, `.pin`, `.availability` and
//! `.request_status`, registered through [`crate::services::UseCases::with_router`]. The native
//! composer and agent tools reach the same use cases through the gateway; host inference
//! pipelines use [`InferenceRouter::route_auxiliary`] and [`InferenceRouter::admit_request`].
//!
//! # Intent (ROUTE-19)
//!
//! * `Tier {Max|High|Mid|Low}` pools the members configured in Settings
//!   (`routing.tier<Tier>.members`, in order). Membership is editable configuration: any
//!   catalog model, including a custom one, joins a tier by being listed there.
//! * `Model {modelId}` names that catalog model directly. A named model is never translated to a
//!   tier: when it is not eligible the request waits or is refused, it does not fail over.
//! * `OpenRouterAuto` is OpenRouter's own automatic upstream route (an `OpenRouterChat` catalog
//!   entry with upstream `openrouter/auto`), never a synonym for a DevWorks tier.
//!
//! # Eligibility (ROUTE-03, ROUTE-13, ROUTE-20)
//!
//! A pooled model is eligible when it is enabled, declares every required capability (`vision`,
//! `tools`, `context:N` with a declared window) and the requested effort, has a usable
//! connection of its endpoint profile ([`ConnectionDirectory`]: connected, and listing the model
//! once discovered) and is not cooling. `Default` effort delegates to the provider; an explicit
//! effort is honoured exactly or the model is ineligible, so a failover can never drop it.
//! Unknown capability labels are refused, never ignored.
//!
//! # Pins (ROUTE-14, ROUTE-15)
//!
//! Every session has at most one pin. `router.route` reuses an eligible pin for the same intent
//! and effort (`NoChange`); otherwise it allocates one in the same unit of work as the receipt:
//! a tier takes one durable round-robin turn over its eligible members, a named model or Open
//! Router Auto takes that route. A pin whose model became ineligible fails over to an eligible
//! replacement of the same pool (never another tier) and keeps it after the original recovers.
//! An effort change on a model that declares the new effort keeps the model. `router.pin` is
//! the explicit change at a request boundary (expected pin revision; capabilities of the last
//! routed request still apply). When nothing in the pool is eligible the command is refused and
//! the pin is unchanged: `CapacityWait` while a cooldown is the only obstacle (the earliest
//! expiry is `router.availability`'s `retryAtMs`), `CredentialsRequired` when a sign-in would
//! help, `UnsupportedCapability` otherwise. Restoring a pin never resumes feature or issue work.
//!
//! # Auxiliary requests
//!
//! Titles, compaction, enrichment, embeddings and vision extraction use
//! [`InferenceRouter::route_auxiliary`]: the session's pin when it is eligible, otherwise the
//! first eligible member of the requested pool, without taking a round-robin turn or writing
//! the session's pin.
//!
//! # Retained CodeGraph behavior
//!
//! `LlmProvidersController.ListProviderModels` maps to `router.catalog`; its provider and
//! analysis/review/assistant configuration endpoints map to `router.connections` and
//! `router.configure` (the provider-connection item) and to the routing settings.
pub mod select;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, router as ops};
use crate::ports::router::ConnectionDirectory;
use crate::ports::storage::{
    ConfigScope, ConfigurationRead, NewEvent, NewOperation, PIN_REASONS, ReadSnapshot, RouterRead,
    StorageError, StoredCooldown, StoredPin, StoredRouteModel, StoredRouteRequest, StoredSession,
    WriteTransaction,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::records::{
    EffortName, EndpointKind, OperationRef, RouteAvailability, RouteChoice, RouteIntent,
    RouteModel, TierName,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    ActorId, CorrelationId, Counter, DomainErrorCode, DomainFault, EventId, Id, OperationId,
    ProjectId, RetryAdvice, RevisionDimension, RootId,
};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{
    Failure, Refusal, counter, decided, label, not_found, project_visible, query_fault, rev,
    visible_session,
};
use crate::settings::catalog::descriptor;
use crate::settings::model::{chain, current_chain};
use select::{Assessed, Need, Unavailable};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The operations [`InferenceRouter`] is registered for.
pub const OPERATIONS: [&str; 5] = [
    "router.catalog",
    "router.route",
    "router.pin",
    "router.availability",
    "router.request_status",
];

/// Tiers in selector order, with their membership setting.
pub const TIERS: [(&str, &str); 4] = [
    ("Max", "routing.tierMax.members"),
    ("High", "routing.tierHigh.members"),
    ("Mid", "routing.tierMid.members"),
    ("Low", "routing.tierLow.members"),
];
/// History aggregate of a session's route pin (its ID is the session ID).
pub const PIN_AGGREGATE: &str = "route_pin";
/// History event of a written pin.
pub const PIN_EVENT: &str = "RoutePinned";
/// Journal capability of a logical inference request.
pub const REQUEST_CAPABILITY: &str = "router.request";
/// The most catalog rows the router reads for one decision.
const CATALOG_LIMIT: u16 = 500;

/// What an inference request is for. Only a conversation request (`router.route`) allocates or
/// replaces a session pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Conversation,
    Compaction,
    Title,
    Enrichment,
    Embedding,
    Vision,
}
impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "Conversation",
            Self::Compaction => "Compaction",
            Self::Title => "Title",
            Self::Enrichment => "Enrichment",
            Self::Embedding => "Embedding",
            Self::Vision => "Vision",
        }
    }
}

/// An auxiliary route request (no pin is written).
#[derive(Clone, Debug)]
pub struct AuxiliaryRequest {
    pub purpose: Purpose,
    /// The conversation it serves, whose eligible pin it reuses.
    pub session_id: Option<Id>,
    pub intent: RouteIntent,
    pub effort: EffortName,
    pub required_capabilities: Vec<String>,
}
/// The route an auxiliary request takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuxiliaryRoute {
    pub model_id: Id,
    pub connection_id: Id,
    /// `EffortName` wire names.
    pub requested_effort: String,
    pub effective_effort: String,
    /// The session pin it reused, if any.
    pub pin_id: Option<Id>,
}
/// Why a route is unavailable, with the pool's block codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteBlocked {
    pub unavailable: Unavailable,
    pub codes: Vec<&'static str>,
}

/// What a logical-request pipeline records when it admits an inference request routed by this
/// service (the provider transports bind here).
#[derive(Clone, Debug)]
pub struct RequestAdmission {
    pub request_id: Id,
    pub purpose: Purpose,
    pub session_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub model_id: Id,
    pub connection_id: Id,
    pub pin_id: Option<Id>,
    /// Digest of the normalized request.
    pub args_digest: String,
}

pub struct InferenceRouter {
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    connections: Arc<dyn ConnectionDirectory>,
}

// ---------------------------------------------------------------------------------------------
// Wire helpers

fn wire<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn parse_wire<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|_| StorageError::InvalidRecord(format!("stored router value {s}")))
}
/// The canonical stored form of an intent (fields are emitted in declaration order).
pub fn intent_key(intent: &RouteIntent) -> String {
    serde_json::to_string(intent).unwrap_or_default()
}
fn intent_of(key: &str) -> Result<RouteIntent, StorageError> {
    serde_json::from_str(key).map_err(|_| StorageError::InvalidRecord("stored route intent".into()))
}
fn wire_label(s: &str) -> BoundedText<1, 256> {
    label(s, "unknown")
}

/// The wire choice of a stored pin.
pub fn choice_of(pin: &StoredPin) -> Result<RouteChoice, StorageError> {
    Ok(RouteChoice {
        model_id: pin.model_id.clone(),
        connection_id: pin.connection_id.clone(),
        intent: intent_of(&pin.intent)?,
        requested_effort: parse_wire(&pin.requested_effort)?,
        effective_effort: parse_wire(&pin.effective_effort)?,
        pin_id: pin.pin_id.clone(),
        pin_revision: rev(pin.revision),
        replaced_pin_id: pin.replaced_pin_id.clone(),
    })
}

/// The wire record of a catalog model with its tier memberships.
pub fn route_model(
    m: &StoredRouteModel,
    members: &BTreeMap<&'static str, Vec<Id>>,
) -> Result<RouteModel, StorageError> {
    let mut tiers = Vec::new();
    for (tier, _) in TIERS {
        if members.get(tier).is_some_and(|ids| ids.contains(&m.id)) {
            tiers.push(parse_wire::<TierName>(tier)?);
        }
    }
    let efforts = m
        .efforts
        .iter()
        .map(|e| parse_wire::<EffortName>(e))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RouteModel {
        id: m.id.clone(),
        display_name: wire_label(&m.display_name),
        endpoint_profile: parse_wire::<EndpointKind>(&m.endpoint_profile)?,
        upstream_model_id: BoundedText(m.upstream_model_id.clone()),
        enabled: m.enabled,
        tier_memberships: BoundedList(tiers),
        efforts: BoundedList(efforts),
        image_input: m.image_input,
        tools: m.tools,
        revision: rev(m.revision),
    })
}

// ---------------------------------------------------------------------------------------------
// Routing facts

/// Everything one routing decision reads, gathered once.
struct Facts {
    members: BTreeMap<&'static str, Vec<Id>>,
    models: BTreeMap<String, StoredRouteModel>,
    /// Display order.
    ordered: Vec<Id>,
    cooling: BTreeMap<String, i64>,
}

/// The configured members of every tier (Settings, workspace scope), in order.
pub fn tier_members(
    read: &dyn ConfigurationRead,
) -> Result<BTreeMap<&'static str, Vec<Id>>, StorageError> {
    let layers = current_chain(read, &ConfigScope::Workspace)?;
    let mut out = BTreeMap::new();
    for (tier, key) in TIERS {
        let d = descriptor(key).expect("catalog setting");
        let entries = chain(d, &layers).map_err(|m| {
            StorageError::InvalidRecord(format!("{} stored under another schema", m.key))
        })?;
        let value = entries
            .first()
            .map(|e| e.value.value.0.clone())
            .unwrap_or_else(|| d.default_value());
        let ids = value
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().and_then(|s| Id::new(s).ok()))
                    .collect()
            })
            .unwrap_or_default();
        out.insert(tier, ids);
    }
    Ok(out)
}

fn all_models(read: &dyn RouterRead) -> Result<Vec<StoredRouteModel>, StorageError> {
    let mut out: Vec<StoredRouteModel> = Vec::new();
    loop {
        let after = out.last().map(|m| (m.position, m.id.as_str()));
        let page = read.route_models(after, CATALOG_LIMIT)?;
        let full = page.len() == CATALOG_LIMIT as usize;
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}

fn facts(
    config: &dyn ConfigurationRead,
    router: &dyn RouterRead,
    now: i64,
) -> Result<Facts, StorageError> {
    facts_with(tier_members(config)?, router, now)
}
fn facts_with(
    members: BTreeMap<&'static str, Vec<Id>>,
    router: &dyn RouterRead,
    now: i64,
) -> Result<Facts, StorageError> {
    let rows = all_models(router)?;
    let ordered = rows.iter().map(|m| m.id.clone()).collect();
    let models = rows
        .into_iter()
        .map(|m| (m.id.as_str().to_owned(), m))
        .collect();
    let cooldowns: Vec<StoredCooldown> = router.cooldowns()?;
    Ok(Facts {
        members,
        models,
        ordered,
        cooling: select::active_cooldowns(&cooldowns, now),
    })
}

impl Facts {
    /// The model IDs `intent` pools, in allocation order.
    fn pool(&self, intent: &RouteIntent) -> Vec<Id> {
        match intent {
            RouteIntent::Tier { tier } => self
                .members
                .get(wire(tier).as_str())
                .cloned()
                .unwrap_or_default(),
            RouteIntent::Model { model_id } => vec![model_id.clone()],
            RouteIntent::OpenRouterAuto {} => self
                .ordered
                .iter()
                .filter(|id| {
                    self.models.get(id.as_str()).is_some_and(|m| {
                        m.endpoint_profile == select::OPEN_ROUTER_PROFILE
                            && m.upstream_model_id == select::OPEN_ROUTER_AUTO_UPSTREAM
                    })
                })
                .cloned()
                .collect(),
        }
    }
    fn assess_pool(
        &self,
        intent: &RouteIntent,
        connections: &[crate::ports::router::ConnectionStatus],
        need: &Need,
        preferred: Option<(&Id, &Id)>,
    ) -> Vec<Assessed> {
        let mut seen = Vec::new();
        self.pool(intent)
            .into_iter()
            .filter(|id| {
                // A member listed twice takes one place.
                let fresh = !seen.contains(id);
                seen.push(id.clone());
                fresh
            })
            .map(|id| {
                let conn = preferred.filter(|(m, _)| *m == &id).map(|(_, c)| c);
                select::assess(
                    &id,
                    self.models.get(id.as_str()),
                    connections,
                    &self.cooling,
                    need,
                    conn,
                )
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Refusals

const ROUTE: &str = "route";
const PIN: &str = "route_pin";
const MODEL: &str = "route_model";
const CAPABILITY: &str = "capability";
const REQUEST: &str = "route_request";

fn invalid(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None).into()
}

/// The refusal of a route with no eligible member.
fn blocked(intent: &RouteIntent, pool: &[Assessed], session: Option<&Id>) -> Failure {
    if let RouteIntent::Model { model_id } = intent
        && pool
            .iter()
            .all(|a| a.blocks == [select::Block::NotInCatalog])
    {
        return not_found(MODEL, model_id).into();
    }
    let (code, retry) = match select::unavailable(pool) {
        Unavailable::Cooling(_) | Unavailable::Connection => (
            DomainErrorCode::CapacityWait,
            RetryAdvice::AfterBlockerClears,
        ),
        Unavailable::Credentials => (
            DomainErrorCode::CredentialsRequired,
            RetryAdvice::AfterBlockerClears,
        ),
        Unavailable::EmptyPool | Unavailable::Unsupported => {
            (DomainErrorCode::UnsupportedCapability, RetryAdvice::Never)
        }
    };
    Refusal::new(code, ROUTE, session.cloned())
        .retry(retry)
        .into()
}

/// Who may route or repin a session: the user, the host, or the session's own agent or creator
/// (an agent only from an invocation bound to that session).
fn may_route(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    session: &StoredSession,
) -> Result<(), Failure> {
    let denied = || {
        Failure::from(Refusal::new(
            DomainErrorCode::PermissionDenied,
            ROUTE,
            Some(session.id.clone()),
        ))
    };
    if tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind()) {
        return Err(denied());
    }
    match ctx.principal_kind() {
        PrincipalKind::Human | PrincipalKind::Host => Ok(()),
        kind => {
            let principal = ctx.principal();
            if principal != &session.agent_id && principal != &session.created_by {
                return Err(denied());
            }
            if kind == PrincipalKind::Agent && ctx.session_id() != Some(&session.id) {
                return Err(denied());
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The service

/// How a resolution changed the pin.
enum Resolution {
    Kept(StoredPin),
    Written(StoredPin),
}

impl InferenceRouter {
    pub fn new(
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        connections: Arc<dyn ConnectionDirectory>,
    ) -> Self {
        Self {
            ids,
            clock,
            connections,
        }
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// The session a command routes, after scope, ownership, open and (optionally) epoch checks.
    fn session_for(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        session_id: &Id,
        expected_epoch: Option<&Counter>,
    ) -> Result<StoredSession, Failure> {
        let session = visible_session(tx.sessions(), ctx, session_id)?;
        may_route(tx, ctx, &session)?;
        if session.closed() {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                crate::services::sessions::SESSION_CLOSED,
                Some(session.id.clone()),
            )
            .into());
        }
        if let Some(e) = expected_epoch
            && e.get() != session.context_epoch
        {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                crate::services::sessions::EPOCH_CHANGED,
                Some(session.id.clone()),
            )
            .retry(RetryAdvice::RefreshAndNewRequest)
            .into());
        }
        Ok(session)
    }

    /// Resolves the session's route for `intent` and `need` in the command's unit of work.
    /// `explicit` is `router.pin`: an unchanged selection is kept as it is.
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &self,
        tx: &mut dyn WriteTransaction,
        session: &StoredSession,
        actor: &ActorId,
        current: Option<StoredPin>,
        intent: &RouteIntent,
        need: &Need,
        explicit: bool,
    ) -> Result<Resolution, Failure> {
        let now = tx.now_ms();
        let key = intent_key(intent);
        let members = tier_members(tx.configuration())?;
        let f = facts_with(members, tx.router(), now)?;
        let connections = self.connections.connections();
        let same_intent = current.as_ref().is_some_and(|p| p.intent == key);
        let same_effort = current
            .as_ref()
            .is_some_and(|p| p.requested_effort == need.effort);
        if explicit && same_intent && same_effort {
            return Ok(Resolution::Kept(current.expect("pin")));
        }
        let preferred = current
            .as_ref()
            .filter(|_| same_intent)
            .map(|p| (&p.model_id, &p.connection_id));
        let pool = f.assess_pool(intent, &connections, need, preferred);
        let pinned = current.as_ref().filter(|_| same_intent).and_then(|p| {
            pool.iter()
                .find(|a| a.model_id == p.model_id && a.eligible())
                .map(|a| (p, a))
        });
        // The pinned model still serves this intent and need.
        if let Some((p, a)) = pinned {
            let connection = a.connection.clone().expect("eligible");
            if same_effort && connection == p.connection_id {
                if p.required_capabilities == need.labels {
                    return Ok(Resolution::Kept(p.clone()));
                }
                // Same route, but this request needs other capabilities: the pin records the
                // latest request's requirements (an explicit change must still meet them), with
                // the reason it was last routed for unchanged.
                let reason = PIN_REASONS
                    .iter()
                    .copied()
                    .find(|r| *r == p.reason)
                    .unwrap_or("Initial");
                let pin = self.write_pin(
                    tx,
                    (session, actor),
                    current.as_ref(),
                    intent,
                    need,
                    a,
                    reason,
                    true,
                )?;
                return Ok(Resolution::Written(pin));
            }
            let reason = if connection != p.connection_id {
                "ConnectionChanged"
            } else {
                "EffortChanged"
            };
            let keep_id = connection == p.connection_id;
            let pin = self.write_pin(
                tx,
                (session, actor),
                current.as_ref(),
                intent,
                need,
                a,
                reason,
                keep_id,
            )?;
            return Ok(Resolution::Written(pin));
        }
        let eligible: Vec<&Assessed> = pool.iter().filter(|a| a.eligible()).collect();
        if eligible.is_empty() {
            return Err(blocked(intent, &pool, Some(&session.id)));
        }
        let chosen = match intent {
            RouteIntent::Tier { tier } => {
                let tier = wire(tier);
                let turn = tx.router().tier_cursor(&tier)?;
                let pick = eligible[(turn.rem_euclid(eligible.len() as i64)) as usize];
                tx.router().advance_tier_cursor(&tier, turn)?;
                pick
            }
            RouteIntent::Model { .. } | RouteIntent::OpenRouterAuto {} => eligible[0],
        };
        let reason = match &current {
            None => "Initial",
            Some(_) if !same_intent => "SelectionChanged",
            Some(_) if !same_effort => "EffortChanged",
            Some(_) => "Failover",
        };
        let pin = self.write_pin(
            tx,
            (session, actor),
            current.as_ref(),
            intent,
            need,
            chosen,
            reason,
            false,
        )?;
        Ok(Resolution::Written(pin))
    }

    /// Writes the session's pin (compare-and-set on the current revision) and its history fact,
    /// attributed to the principal that routed it.
    #[allow(clippy::too_many_arguments)]
    fn write_pin(
        &self,
        tx: &mut dyn WriteTransaction,
        (session, actor): (&StoredSession, &ActorId),
        current: Option<&StoredPin>,
        intent: &RouteIntent,
        need: &Need,
        chosen: &Assessed,
        reason: &'static str,
        keep_id: bool,
    ) -> Result<StoredPin, Failure> {
        debug_assert!(PIN_REASONS.contains(&reason));
        let now = tx.now_ms();
        let (pin_id, replaced) = match current {
            Some(p) if keep_id => (p.pin_id.clone(), p.replaced_pin_id.clone()),
            Some(p) => (tx.new_id(IdKind::RoutePin), Some(p.pin_id.clone())),
            None => (tx.new_id(IdKind::RoutePin), None),
        };
        let pin = StoredPin {
            session_id: session.id.clone(),
            pin_id,
            revision: current.map_or(1, |p| p.revision + 1),
            model_id: chosen.model_id.clone(),
            connection_id: chosen.connection.clone().expect("eligible"),
            intent: intent_key(intent),
            requested_effort: need.effort.clone(),
            // The effective effort is the requested one: `Default` is left to the provider, and
            // an explicit effort is only routed to a model that declares it.
            effective_effort: need.effort.clone(),
            required_capabilities: need.labels.clone(),
            replaced_pin_id: replaced,
            reason: reason.to_owned(),
            updated_at_ms: now,
        };
        let stored = tx
            .router()
            .put_session_pin(&pin, current.map(|p| p.revision))?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event,
            actor: actor.clone(),
            project_id: session.project_id.clone(),
            root_id: session.root_id.clone(),
            operation_id: None,
            aggregate_kind: PIN_AGGREGATE.into(),
            aggregate_id: session.id.as_str().to_owned(),
            aggregate_revision: stored.revision,
            kind: PIN_EVENT.into(),
            payload: json!({
                "sessionId": session.id.as_str(),
                "pinId": stored.pin_id.as_str(),
                "modelId": stored.model_id.as_str(),
                "connectionId": stored.connection_id.as_str(),
                "intent": serde_json::from_str::<Value>(&stored.intent).unwrap_or(Value::Null),
                "requestedEffort": stored.requested_effort,
                "effectiveEffort": stored.effective_effort,
                "requiredCapabilities": stored.required_capabilities,
                "replacedPinId": stored.replaced_pin_id.as_ref().map(Id::as_str),
                "previousModelId": current.map(|p| p.model_id.as_str()),
                "reason": reason,
            }),
        })?;
        Ok(stored)
    }

    fn route(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::RouteInput,
    ) -> Result<DomainDecision, Failure> {
        let session = Self::session_for(tx, ctx, &i.session_id, Some(&i.expected_context_epoch))?;
        let need = Need::parse(
            &wire(&i.effort),
            i.required_capabilities.0.iter().map(|l| l.0.as_str()),
        )
        .map_err(|_| invalid(CAPABILITY))?;
        let current = tx.router().session_pin(&session.id)?;
        Ok(
            match self.resolve(
                tx,
                &session,
                ctx.principal(),
                current,
                &i.intent,
                &need,
                false,
            )? {
                Resolution::Kept(p) => DomainDecision::NoChange {
                    result: OperationResult::RouterRoute {
                        value: choice_of(&p)?,
                    },
                },
                Resolution::Written(p) => DomainDecision::Applied {
                    result: OperationResult::RouterRoute {
                        value: choice_of(&p)?,
                    },
                },
            },
        )
    }

    fn pin(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::PinInput,
    ) -> Result<DomainDecision, Failure> {
        let session = Self::session_for(tx, ctx, &i.session_id, None)?;
        let Some(current) = tx.router().session_pin(&session.id)? else {
            return Err(not_found(PIN, &session.id).into());
        };
        if current.revision != i.expected_pin_revision.get() {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                PIN,
                session.id.clone(),
                i.expected_pin_revision.clone(),
                rev(current.revision),
            )));
        }
        // The capabilities the session's requests need still apply to an explicit change.
        let need = Need::parse(
            &wire(&i.effort),
            current.required_capabilities.iter().map(String::as_str),
        )
        .map_err(|_| invalid(CAPABILITY))?;
        Ok(
            match self.resolve(
                tx,
                &session,
                ctx.principal(),
                Some(current),
                &i.intent,
                &need,
                true,
            )? {
                Resolution::Kept(p) => DomainDecision::NoChange {
                    result: OperationResult::RouterPin {
                        value: choice_of(&p)?,
                    },
                },
                Resolution::Written(p) => DomainDecision::Applied {
                    result: OperationResult::RouterPin {
                        value: choice_of(&p)?,
                    },
                },
            },
        )
    }

    // -----------------------------------------------------------------------------------------
    // Queries

    fn catalog(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::CatalogInput,
    ) -> Result<OperationResult, Failure> {
        let members = tier_members(snapshot.configuration())?;
        let binding = CursorBinding::new(
            ctx,
            "router.catalog",
            &[if i.include_disabled { "all" } else { "enabled" }],
            1,
        );
        let mut after: Option<(i64, String)> = match &i.page.cursor {
            None => None,
            Some(c) => {
                let at = binding.decode(&c.0)?;
                let (pos, id) = at.split_once('|').ok_or(StorageError::CursorExpired)?;
                Some((
                    pos.parse().map_err(|_| StorageError::CursorExpired)?,
                    id.to_owned(),
                ))
            }
        };
        let limit = i.page.limit.get() as usize;
        let mut items = Vec::new();
        let mut last: Option<(i64, String)> = None;
        let mut more = false;
        'pages: loop {
            let rows = snapshot.router().route_models(
                after.as_ref().map(|(p, id)| (*p, id.as_str())),
                CATALOG_LIMIT,
            )?;
            let full = rows.len() == CATALOG_LIMIT as usize;
            for m in &rows {
                after = Some((m.position, m.id.as_str().to_owned()));
                if !(i.include_disabled || m.enabled) {
                    continue;
                }
                if items.len() == limit {
                    more = true;
                    break 'pages;
                }
                items.push(route_model(m, &members)?);
                last = Some((m.position, m.id.as_str().to_owned()));
            }
            if !full {
                break;
            }
        }
        let next_cursor = match (more, last) {
            (true, Some((p, id))) => Some(BoundedText(binding.encode(&format!("{p}|{id}")))),
            _ => None,
        };
        Ok(OperationResult::RouterCatalog {
            value: Page {
                items: BoundedList(items),
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        })
    }

    /// The truthful availability of `intent` for `effort` and `capabilities` right now, with
    /// `pin` (a session's current pin) and whether it is eligible for them.
    pub fn availability_of(
        &self,
        snapshot: &dyn ReadSnapshot,
        intent: &RouteIntent,
        effort: &EffortName,
        capabilities: &[&str],
        pin: Option<&StoredPin>,
    ) -> Result<RouteAvailability, Failure> {
        let now = self.clock.now_ms().get();
        let need = Need::parse(&wire(effort), capabilities.iter().copied())
            .map_err(|_| invalid(CAPABILITY))?;
        let f = facts(snapshot.configuration(), snapshot.router(), now)?;
        let connections = self.connections.connections();
        let pool = f.assess_pool(intent, &connections, &need, None);
        let eligible: Vec<Id> = pool
            .iter()
            .filter(|a| a.eligible())
            .map(|a| a.model_id.clone())
            .collect();
        let available = !eligible.is_empty();
        let retry_at_ms = if available {
            None
        } else {
            select::earliest_cooldown(&pool).map(counter)
        };
        let pin_eligible = pin.map(|p| {
            p.intent == intent_key(intent)
                && p.requested_effort == need.effort
                && pool
                    .iter()
                    .any(|a| a.model_id == p.model_id && a.eligible())
        });
        Ok(RouteAvailability {
            intent: intent.clone(),
            available,
            eligible_model_ids: BoundedList(eligible),
            blocked_reason_codes: BoundedList(
                select::block_codes(&pool)
                    .into_iter()
                    .map(wire_label)
                    .collect(),
            ),
            retry_at_ms,
            pin: pin.map(choice_of).transpose()?,
            pin_eligible,
            pin_reason: pin.map(|p| wire_label(&p.reason)),
        })
    }

    fn availability(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::AvailabilityInput,
    ) -> Result<OperationResult, Failure> {
        let pin = match &i.session_id {
            Some(id) => {
                let session = visible_session(snapshot.sessions(), ctx, id)?;
                snapshot.router().session_pin(&session.id)?
            }
            None => None,
        };
        let caps: Vec<&str> = i
            .required_capabilities
            .0
            .iter()
            .map(|l| l.0.as_str())
            .collect();
        self.availability_of(snapshot, &i.intent, &i.effort, &caps, pin.as_ref())
            .map(|value| OperationResult::RouterAvailability { value })
    }

    fn request_status(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::RequestStatusInput,
    ) -> Result<OperationResult, Failure> {
        let missing = || Failure::from(not_found(REQUEST, &i.request_id));
        let request = snapshot
            .router()
            .route_request(&i.request_id)?
            .ok_or_else(missing)?;
        if !project_visible(ctx, request.project_id.as_ref()) {
            return Err(missing());
        }
        let op = snapshot
            .operations()
            .operation(&request.operation_id)?
            .ok_or_else(missing)?;
        Ok(OperationResult::RouterRequestStatus {
            value: OperationRef {
                operation_id: op.id,
                state: op.state,
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // Host pipeline API

    /// The route of an auxiliary request (titles, compaction, enrichment, embeddings, vision):
    /// the session's pin when it is eligible for this need, otherwise the first eligible member
    /// of `intent`'s pool. Nothing is written: no round-robin turn, no pin.
    pub fn route_auxiliary(
        &self,
        snapshot: &dyn ReadSnapshot,
        request: &AuxiliaryRequest,
    ) -> Result<Result<AuxiliaryRoute, RouteBlocked>, StorageError> {
        let now = self.clock.now_ms().get();
        let need = Need::parse(
            &wire(&request.effort),
            request.required_capabilities.iter().map(String::as_str),
        )
        .map_err(StorageError::InvalidRecord)?;
        let f = facts(snapshot.configuration(), snapshot.router(), now)?;
        let connections = self.connections.connections();
        if let Some(session) = &request.session_id
            && let Some(pin) = snapshot.router().session_pin(session)?
        {
            let a = select::assess(
                &pin.model_id,
                f.models.get(pin.model_id.as_str()),
                &connections,
                &f.cooling,
                &need,
                Some(&pin.connection_id),
            );
            if a.eligible() {
                return Ok(Ok(AuxiliaryRoute {
                    model_id: pin.model_id,
                    connection_id: a.connection.expect("eligible"),
                    requested_effort: need.effort.clone(),
                    effective_effort: need.effort,
                    pin_id: Some(pin.pin_id),
                }));
            }
        }
        let pool = f.assess_pool(&request.intent, &connections, &need, None);
        Ok(match pool.iter().find(|a| a.eligible()) {
            Some(a) => Ok(AuxiliaryRoute {
                model_id: a.model_id.clone(),
                connection_id: a.connection.clone().expect("eligible"),
                requested_effort: need.effort.clone(),
                effective_effort: need.effort,
                pin_id: None,
            }),
            None => Err(RouteBlocked {
                unavailable: select::unavailable(&pool),
                codes: select::block_codes(&pool),
            }),
        })
    }

    /// Records a logical inference request's durable intent in the operation journal and binds
    /// it for `router.request_status`, in the caller's unit of work.
    pub fn admit_request(
        &self,
        tx: &mut dyn WriteTransaction,
        a: &RequestAdmission,
    ) -> Result<OperationId, StorageError> {
        let operation_id = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: operation_id.clone(),
            root_id: a.root_id.clone(),
            assignment_id: None,
            capability: REQUEST_CAPABILITY.into(),
            schema_version: 1,
            args_digest: a.args_digest.clone(),
            payload: json!({
                "requestId": a.request_id.as_str(),
                "purpose": a.purpose.as_str(),
                "modelId": a.model_id.as_str(),
                "connectionId": a.connection_id.as_str(),
                "sessionId": a.session_id.as_ref().map(Id::as_str),
                "pinId": a.pin_id.as_ref().map(Id::as_str),
            }),
        })?;
        let created_at_ms = tx.now_ms();
        tx.router().record_route_request(&StoredRouteRequest {
            request_id: a.request_id.clone(),
            operation_id: operation_id.clone(),
            purpose: a.purpose.as_str().into(),
            session_id: a.session_id.clone(),
            project_id: a.project_id.clone(),
            model_id: a.model_id.clone(),
            connection_id: a.connection_id.clone(),
            pin_id: a.pin_id.clone(),
            created_at_ms,
        })?;
        Ok(operation_id)
    }

    /// Records a router-reported cooldown of `model` until `until_ms` (the later expiry wins);
    /// an expiry that is not in the future records nothing.
    pub fn record_cooldown(
        &self,
        tx: &mut dyn WriteTransaction,
        model: &Id,
        until_ms: i64,
        reason: &str,
    ) -> Result<Option<StoredCooldown>, StorageError> {
        let now = tx.now_ms();
        if until_ms <= now {
            return Ok(None);
        }
        tx.router()
            .record_cooldown(&StoredCooldown {
                model_id: model.clone(),
                until_ms,
                reason: reason.to_owned(),
                observed_at_ms: now,
            })
            .map(Some)
    }
}

impl QueryHandler for InferenceRouter {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Router(input) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let answered = match input {
            ops::Input::Catalog(i) => self.catalog(snapshot, ctx, i),
            ops::Input::Availability(i) => self.availability(snapshot, ctx, i),
            ops::Input::RequestStatus(i) => self.request_status(snapshot, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let result = answered.map_err(|f| query_fault(f, ROUTE, self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for InferenceRouter {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Router(ops::Input::Route(i)) => self.route(tx, ctx, i),
            OperationInput::Router(ops::Input::Pin(i)) => self.pin(tx, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        decided(tx, outcome)
    }
}
