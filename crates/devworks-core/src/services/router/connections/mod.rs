//! Provider connections, sign-in and native secret storage (ROUTE-09, ROUTE-10, ROUTE-18,
//! SEC-01..05, DW-18) behind `router.connections` and `router.configure`, registered through
//! [`crate::services::UseCases::with_connections`]. The native Settings screen and agent tools
//! reach the same use cases through the gateway; the router reads the same rows through
//! [`ConnectionDirectory`], and provider transports (DW-19) take credentials from
//! [`ConnectionService::access`].
//!
//! # Connections
//!
//! A connection is one endpoint of one profile: Codex subscription (device-code sign-in), Claude
//! subscription (browser PKCE sign-in, disabled until a human accepts [`profiles::RISK_NOTICE`]),
//! Anthropic API key, OpenRouter API key, OpenAI API key, or an OpenAI-compatible endpoint
//! (Ollama, LM Studio; optional key), a compatible embedding endpoint or a SystemOne decision
//! endpoint. Subscription and API credentials never cross: a subscription token is only ever
//! published by its sign-in, an API key only to an API-key profile, and changing a connection's
//! profile or endpoint origin drops its credential. Billing is the profile's, and the router
//! only routes a model to connections of the model's own profile (and, when bound, the one
//! connection it names), so there is no automatic billing crossover.
//!
//! # Secrets (SEC-01, AD 39)
//!
//! Values live only in the [`SecretStore`], under an immutable handle per credential. The order
//! is: write the secret under a new handle, then publish the handle with a compare-and-set of
//! the connection's credential revision (in the command's unit of work, or a host write for a
//! token rotation), then delete the old handle. A handle that was written but not published is
//! deleted by `settle` (refused or replayed commands) or by [`ConnectionService::reconcile`]
//! (crashes), which lists the store's own namespace and never reads a value. API keys and pasted
//! authorization codes enter through the host intake ([`ConnectionService::stage_api_key`],
//! [`ConnectionService::submit_authorization_code`]): only the native UI of the human who will
//! submit the command uses it, and a staged handle is only accepted from that same principal.
//! Nothing secret is written to SQLite, receipts, history, results, logs or diagnostics.
//!
//! # Lifecycle
//!
//! Stored: `Disconnected`, `Connected`, `ReauthenticationRequired`, `Unavailable`. Derived in this
//! process: `Authorizing` (an open sign-in flow), `Refreshing` (a running token refresh) and
//! `Disabled` (a Claude subscription without an effective risk acceptance). A risk acceptance is
//! bound to the exact notice, profile, endpoint, account and store restore epoch; a credential to
//! the restore epoch it was published in, so a restored store needs a new sign-in.
//!
//! # Effects
//!
//! `router.configure` decides in its unit of work and never touches the network there. Network
//! effects run in `prepare` (bounded: device-code start, code exchange, model discovery and the
//! connection test) and are published by `decide`; `settle` resolves what the unit of work did
//! not commit. Device polling and periodic health checks run in [`ConnectionService::tick`],
//! driven by the host. A token refresh is serialized per connection and its rotated tokens are
//! persisted whatever happens to the request that needed them.
mod access;
pub mod auth;
mod configure;
pub mod discovery;
mod health;
pub mod lifecycle;
mod models;
mod secrets;
mod signin;

pub use crate::providers::openrouter::open_router_request_fields;
pub use crate::providers::profiles;
pub use health::discovered_model_id;
use health::{CheckBasis, Checked, apply_check, clear_profile_cooldowns};
use models::{configure_decision_model, configure_model, configure_tier};
use signin::Prep;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::locks::LockExt;
use crate::operations::{OperationInput, router as ops};
use crate::ports::http::{Entropy, HttpTransport, base64url};
use crate::ports::router::{ConnectionDirectory, ConnectionState, ConnectionStatus};
use crate::ports::secrets::{
    Secret, SecretError, SecretKey, SecretRef, SecretResolver, SecretStore, SecretUnavailable,
    reference_handle,
};
use crate::ports::storage::{
    ApplicationStore, DiscoveredModel, NewEvent, ReadOptions, ReadSnapshot, RouterRead,
    SettingsSecretState, StorageError, StoredConnection, StoredDecisionModel, StoredRiskAcceptance,
    StoredRouteModel, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    AccountAction, ConnectionLifecycle, ConnectionRecord, DecisionModel, EndpointKind, ItemState,
    MutationSummary, OpenRouterConstraints, RouteModel, RouterConfigAction, SignInChallenge,
    SignInMethod, TierName,
};
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Url};
use crate::protocol::{
    ActorId, CommandReply, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, RetryAdvice,
    RevisionDimension,
};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, Refusal, counter, decided, label, query_fault, rev};
use crate::services::router::{
    TIERS, all_models, catalog_tier_members, invalid, latest, parse_wire, select, tier_members,
    wire,
};
use auth::{AuthFailure, Credential, DeviceCode, Tokens};
use discovery::{Discovery, ListFailure};
use profiles::{AuthEndpoints, Profile, profile};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

/// The operations [`ConnectionService`] is registered for.
pub const OPERATIONS: [&str; 2] = ["router.connections", "router.configure"];
/// History aggregate of a connection.
pub const CONNECTION_AGGREGATE: &str = "provider_connection";
/// History aggregate of a catalog model edited through `router.configure`.
pub const MODEL_AGGREGATE: &str = "route_model";
/// History aggregate of a SystemOne decision model.
pub const DECISION_AGGREGATE: &str = "decision_model";
/// A staged API key not published within this long is deleted by [`ConnectionService::reconcile`].
pub const STAGED_TTL_MS: i64 = 30 * 60 * 1000;
/// Reads of a credential that was replaced while it was read before giving up.
const CREDENTIAL_READ_ATTEMPTS: usize = 3;
/// Connected connections rediscover their models this often (reference
/// `ModelListRefreshInterval`).
pub const DISCOVERY_INTERVAL_MS: i64 = 60 * 60 * 1000;
/// An unavailable connection is tested again this often.
pub const UNAVAILABLE_RECHECK_MS: i64 = 60 * 1000;
/// How often the host reconciles orphaned secret handles.
pub const RECONCILE_INTERVAL_MS: i64 = 60 * 60 * 1000;
/// Consecutive refreshes with an unknown outcome before reauthentication is required (a
/// rotated refresh token is never retried without bound).
const AMBIGUOUS_REFRESH_LIMIT: u32 = 2;
/// Rows read per repository page (its maximum).
use crate::ports::storage::MAX_PAGE;

const CONNECTION: &str = "connection";
const FLOW: &str = "sign_in_flow";
const CREDENTIAL: &str = "credential_ref";

// ---------------------------------------------------------------------------------------------
// In-process state

enum FlowKind {
    Device {
        code: DeviceCode,
        next_poll_ms: i64,
    },
    Browser {
        verifier: Secret,
        state: Secret,
        authorize_url: String,
        pasted: Option<Secret>,
    },
}
struct Flow {
    id: Id,
    connection_id: Id,
    profile: String,
    endpoint: String,
    /// The connection's credential revision when the sign-in began; publishing is a
    /// compare-and-set on it.
    credential_revision: i64,
    started_by: ActorId,
    kind: FlowKind,
    expires_at_ms: i64,
    /// Set by `decide` of the command that began it; `settle` activates a decided flow of an
    /// applied command and drops every other one (a refusal, a replay, a rollback).
    decided: bool,
    active: bool,
    /// A poll or exchange of this flow is running (one at a time).
    busy: bool,
}

/// A handle written to the secret store and not yet published (or deleted).
struct Staged {
    principal: ActorId,
    /// An API key from the host intake. Only these may be named by a `Connection` action: the
    /// other staged handles (exchanged or rotated subscription tokens) are internal, so a
    /// subscription token can never be published as an API key.
    api_key: bool,
    created_at_ms: i64,
}

#[derive(Default)]
struct Live {
    flows: BTreeMap<String, Flow>,
    staged: BTreeMap<String, Staged>,
    refreshing: BTreeSet<String>,
    /// The last sign-in or refresh failure per connection, shown until the next attempt.
    failures: BTreeMap<String, String>,
    /// Refreshes with an unknown outcome per connection, for the credential revision they were
    /// made with (another credential starts again from zero).
    ambiguous: BTreeMap<String, (i64, u32)>,
    ticking: bool,
    last_reconcile_ms: Option<i64>,
    /// Why the secret store last refused (locked, missing, denied); cleared by its next success.
    secret_unavailable: Option<String>,
}

/// What a transport receives for one request (DW-19).
pub enum AccessCredential {
    /// A keyless compatible endpoint.
    None,
    ApiKey(Secret),
    /// An OAuth access token, with the account the provider needs beside it.
    Bearer {
        token: Secret,
        account: Option<String>,
    },
}
impl std::fmt::Debug for AccessCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::ApiKey(_) => "ApiKey(<redacted>)",
            Self::Bearer { .. } => "Bearer(<redacted>)",
        })
    }
}
#[derive(Debug)]
pub struct Access {
    pub connection_id: Id,
    pub profile: String,
    pub endpoint: String,
    pub credential_revision: i64,
    pub credential: AccessCredential,
    /// The OpenRouter constraints of the same read, for the transport to send with each
    /// request ([`open_router_request_fields`]).
    pub open_router: Option<OpenRouterConstraints>,
}
/// Why no credential is available. Never carries a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessError {
    NotFound,
    /// No credential is configured.
    Disconnected,
    /// The provider refused the credential, or it is missing or was restored: sign in again.
    ReauthenticationRequired(String),
    /// The provider or the secret store is temporarily unavailable.
    Unavailable(String),
    /// A Claude subscription without an effective risk acceptance.
    Disabled,
}

/// Why the host intake refused a pasted code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntakeError {
    UnknownFlow,
    NotBrowserFlow,
}

pub struct ConnectionService {
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    store: Arc<dyn ApplicationStore>,
    http: Arc<dyn HttpTransport>,
    secrets: Arc<dyn SecretStore>,
    entropy: Arc<dyn Entropy>,
    endpoints: AuthEndpoints,
    live: Mutex<Live>,
    refresh_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    lifecycle: lifecycle::Lifecycle,
    embedding_spaces: OnceLock<Arc<super::embedding_spaces::EmbeddingSpaceService>>,
}

// ---------------------------------------------------------------------------------------------
// Wire helpers

fn text(s: &str) -> Option<BoundedText<1, 4096>> {
    let t: String = s.chars().take(4096).collect();
    (!t.is_empty()).then_some(BoundedText(t))
}
fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
fn summary(id: &Id, revision: i64, changed: bool) -> OperationResult {
    OperationResult::RouterConfigure {
        value: MutationSummary {
            id: id.clone(),
            revision: rev(revision),
            changed,
        },
    }
}
fn applied(id: &Id, revision: i64) -> DomainDecision {
    DomainDecision::Applied {
        result: summary(id, revision, true),
    }
}
fn unchanged(id: &Id, revision: i64) -> DomainDecision {
    DomainDecision::NoChange {
        result: summary(id, revision, false),
    }
}

/// Every connection, in ID order.
fn all_connections(read: &dyn RouterRead) -> Result<Vec<StoredConnection>, StorageError> {
    let mut out: Vec<StoredConnection> = Vec::new();
    loop {
        let after = out.last().map(|c| c.id.as_str().to_owned());
        let page = read.connections(after.as_deref(), MAX_PAGE)?;
        let full = page.len() == MAX_PAGE as usize;
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}
fn all_decision_models(read: &dyn RouterRead) -> Result<Vec<StoredDecisionModel>, StorageError> {
    let mut out: Vec<StoredDecisionModel> = Vec::new();
    loop {
        let after = out.last().map(|m| m.id.as_str().to_owned());
        let page = read.decision_models(after.as_deref(), MAX_PAGE)?;
        let full = page.len() == MAX_PAGE as usize;
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}
/// Whether any catalog or decision model is bound to `connection`.
fn bound_models(read: &dyn RouterRead, connection: &Id) -> Result<bool, StorageError> {
    Ok(all_models(read)?
        .iter()
        .any(|m| m.connection_id.as_ref() == Some(connection))
        || all_decision_models(read)?
            .iter()
            .any(|m| &m.connection_id == connection))
}
/// Every credential handle a connection publishes.
fn referenced(read: &dyn RouterRead) -> Result<BTreeSet<String>, StorageError> {
    Ok(all_connections(read)?
        .into_iter()
        .filter_map(|c| c.credential_ref.map(|r| r.as_str().to_owned()))
        .collect())
}

fn event(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    aggregate: (&str, &Id, i64),
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: aggregate.0.to_owned(),
        aggregate_id: aggregate.1.as_str().to_owned(),
        aggregate_revision: aggregate.2.max(1),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}
fn next_account_revision(current: i64) -> Result<i64, StorageError> {
    current
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("provider account identity exhausted".into()))
}

/// Host-observed access rejection, committed by the native output owner in its fenced unit.
/// A late response cannot mark a replacement credential, account, endpoint or profile unusable.
/// The first rejection of a credential notifies the user; a rejected run root's dispatch
/// (`root`) is held until the connection is repaired.
pub fn record_inference_rejection(
    tx: &mut dyn WriteTransaction,
    basis: &crate::providers::ProviderAccessBasis,
    status: u16,
    root: Option<&crate::protocol::RootId>,
) -> Result<bool, StorageError> {
    if !matches!(status, 401 | 403) {
        return Ok(false);
    }
    let Some(mut current) = tx.router().connection(&basis.connection_id)? else {
        return Ok(false);
    };
    if current.credential_revision != basis.credential_revision
        || current.account_revision != basis.account_revision
        || current.profile != format!("{:?}", basis.profile)
        || crate::fingerprint::sha256_hex(current.endpoint.as_bytes()) != basis.endpoint_digest
    {
        return Ok(false);
    }
    if current.lifecycle == "ReauthenticationRequired" {
        if let Some(root) = root {
            crate::services::runs::hold_for_credentials(tx, root, &current)?;
        }
        return Ok(false);
    }
    let expected = current.revision;
    current.lifecycle = "ReauthenticationRequired".into();
    current.state_reason = Some(format!(
        "the provider rejected current access (HTTP {status}); inspect the connection or sign in again"
    ));
    current.healthy = Some(false);
    current.updated_at_ms = tx.now_ms();
    let saved = tx.router().put_connection(&current, Some(expected))?;
    let host = tx.store().host_actor.clone();
    event(
        tx,
        &host,
        (CONNECTION_AGGREGATE, &saved.id, saved.revision),
        "InferenceAccessRejected",
        connection_payload(&saved),
    )?;
    crate::services::notifications::credentials_rejected(tx, &saved)?;
    if let Some(root) = root {
        crate::services::runs::hold_for_credentials(tx, root, &saved)?;
    }
    Ok(true)
}

/// The secret-free history payload of a connection.
fn connection_payload(c: &StoredConnection) -> Value {
    json!({
        "connectionId": c.id.as_str(),
        "name": c.name,
        "profile": c.profile,
        "endpoint": c.endpoint,
        "credentialRef": c.credential_ref.as_ref().map(Id::as_str),
        "credentialRevision": c.credential_revision,
        "accountRevision": c.account_revision,
        "lifecycle": c.lifecycle,
        "subscriptionRiskAccepted": c.risk.is_some(),
        "enabled": c.enabled,
        "openRouter": c.open_router.as_deref().and_then(|j| serde_json::from_str::<Value>(j).ok()),
    })
}

/// Whether `c`'s risk acceptance still covers it (exact notice, profile, endpoint, account and
/// restore epoch).
pub fn risk_effective(c: &StoredConnection, restore_epoch: i64) -> bool {
    c.risk.as_ref().is_some_and(|r| {
        r.notice_sha256 == profiles::risk_notice_sha256()
            && r.restore_epoch == restore_epoch
            && r.binding
                == profiles::risk_binding(&c.profile, &c.endpoint, c.account_digest.as_deref())
    })
}

/// Clears what was learned with a credential or account that `next` no longer uses: the model
/// list (and when it was made), the health and the last test. Another key or account may see
/// other models; keeping the old list would reject them as undiscovered until the next listing.
fn forget_credential_facts(next: &mut StoredConnection) {
    next.discovered = None;
    next.discovered_at_ms = None;
    next.healthy = None;
    next.checked_at_ms = None;
}

/// The lifecycle a connection is in with no credential.
fn without_credential(p: &Profile) -> &'static str {
    if p.needs_credential() {
        "Disconnected"
    } else {
        "Connected"
    }
}

// ---------------------------------------------------------------------------------------------
// Prepared work

/// The stored OpenRouter constraints of a connection.
fn constraints_of(c: &StoredConnection) -> Option<OpenRouterConstraints> {
    c.open_router
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
}

/// The router's view of `c` with its model constraints (the state is filled in by the caller).
fn status_filter(c: &StoredConnection) -> ConnectionStatus {
    let (allowed, excluded) = constraints_of(c).map_or((Vec::new(), Vec::new()), |o| {
        (
            o.allowed_models.0.into_iter().map(|m| m.0).collect(),
            o.excluded_models.0.into_iter().map(|m| m.0).collect(),
        )
    });
    ConnectionStatus {
        id: c.id.clone(),
        profile: c.profile.clone(),
        state: ConnectionState::Connected,
        discovered: c
            .discovered
            .as_ref()
            .map(|d| d.iter().map(|m| m.id.clone()).collect()),
        allowed,
        excluded,
    }
}

/// Validates constraints a user configured: non-empty patterns without whitespace, positive
/// throughput and latency preferences.
fn check_constraints(o: &OpenRouterConstraints) -> Result<(), &'static str> {
    for m in o.allowed_models.0.iter().chain(o.excluded_models.0.iter()) {
        if m.0.trim().is_empty() || m.0.chars().any(char::is_whitespace) {
            return Err("open_router_model_pattern");
        }
    }
    if let Some(p) = &o.provider {
        for q in [&p.preferred_min_throughput, &p.preferred_max_latency]
            .into_iter()
            .flatten()
        {
            let v = serde_json::to_value(q).unwrap_or(Value::Null);
            let positive = v.as_object().is_some_and(|m| {
                !m.is_empty() && m.values().all(|x| x.as_f64().is_some_and(|n| n > 0.0))
            });
            if !positive {
                return Err("open_router_preference");
            }
        }
    }
    Ok(())
}

/// The stored (canonical JSON) form of constraints.
fn canonical_constraints(o: &OpenRouterConstraints) -> Result<String, Failure> {
    serde_json::to_string(o).map_err(|_| invalid("open_router"))
}

// ---------------------------------------------------------------------------------------------
// The service

impl ConnectionService {
    /// Installed by the native composition alongside its accounted qualification consumer.
    pub fn install_embedding_spaces(
        &self,
        service: Arc<super::embedding_spaces::EmbeddingSpaceService>,
    ) -> bool {
        self.embedding_spaces.set(service).is_ok()
    }
    pub fn new(
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        store: Arc<dyn ApplicationStore>,
        http: Arc<dyn HttpTransport>,
        secrets: Arc<dyn SecretStore>,
        entropy: Arc<dyn Entropy>,
    ) -> Self {
        Self {
            ids,
            clock,
            store,
            http,
            secrets,
            entropy,
            endpoints: AuthEndpoints::default(),
            live: Mutex::new(Live::default()),
            refresh_locks: Mutex::new(BTreeMap::new()),
            lifecycle: Default::default(),
            embedding_spaces: OnceLock::new(),
        }
    }
    /// Replaces the subscription OAuth endpoints (tests point them at a local fake).
    pub fn with_endpoints(mut self, endpoints: AuthEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }
    pub fn endpoints(&self) -> &AuthEndpoints {
        &self.endpoints
    }
    /// The secret facility in use (its name is shown in Settings).
    pub fn secret_store_name(&self) -> &'static str {
        self.secrets.name()
    }
    /// Why the secret store refused its last request, if it did (locked, missing or denied):
    /// a structured state every path (staging, sign-in exchange, refresh, background sign-in,
    /// resolution) records, so Settings can offer the session-only fallback without reading
    /// any message text. Cleared by the next successful secret-store request.
    pub fn secret_store_unavailable(&self) -> Option<String> {
        self.live().secret_unavailable.clone()
    }
    /// Records the availability a secret-store result shows.
    fn noted<T>(&self, r: Result<T, SecretError>) -> Result<T, SecretError> {
        let mut live = self.live();
        match &r {
            Ok(_) => live.secret_unavailable = None,
            Err(SecretError::Unavailable(why)) => live.secret_unavailable = Some(why.clone()),
            Err(SecretError::Denied) => {
                live.secret_unavailable = Some("access to the secret store was denied".into())
            }
            Err(_) => {}
        }
        r
    }

    /// Writes `tokens` as an internal (subscription-token) staged handle, exactly as a sign-in
    /// exchange stages them. A test seam: it lets a test prove such a handle can never be
    /// published as an API key.
    #[cfg(any(test, feature = "contract-suite"))]
    pub fn stage_tokens_for_test(
        &self,
        principal: &ActorId,
        tokens: Tokens,
    ) -> Result<Id, SecretError> {
        self.write_handle(principal, false, &Credential::OAuth(tokens))
    }

    fn now(&self) -> i64 {
        self.clock.now_ms().get()
    }
    fn namespace(&self) -> String {
        self.store.info().store_id.0.as_str().to_owned()
    }
    fn restore_epoch(&self) -> i64 {
        self.store.info().restore_epoch
    }
    fn key(&self, handle: &Id) -> SecretKey {
        SecretKey::new(self.namespace(), handle.clone())
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
    fn random(&self) -> Secret {
        let mut buf = [0u8; 32];
        self.entropy.fill(&mut buf);
        let s = Secret::new(base64url(&buf));
        buf.iter_mut().for_each(|b| *b = 0);
        s
    }
    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.locked()
    }
    fn read<T>(
        &self,
        work: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), work)
    }

    // -----------------------------------------------------------------------------------------
    // Host intake (native UI of the human principal only)

    // -----------------------------------------------------------------------------------------
    // Effective state

    fn effective(
        &self,
        c: &StoredConnection,
        live: &Live,
        restore_epoch: i64,
        now: i64,
    ) -> (ConnectionState, Option<String>) {
        if !c.enabled {
            return (
                ConnectionState::Disabled,
                Some("the connection is turned off".into()),
            );
        }
        let Some(p) = profile(&c.profile) else {
            return (
                ConnectionState::Disabled,
                Some("unknown endpoint profile".into()),
            );
        };
        if p.risk_gated && !risk_effective(c, restore_epoch) {
            return (
                ConnectionState::Disabled,
                Some("the subscription risk notice is not accepted for this connection".into()),
            );
        }
        if live
            .flows
            .values()
            .any(|f| f.active && f.connection_id == c.id && f.expires_at_ms > now)
        {
            return (
                ConnectionState::Authorizing,
                live.failures.get(c.id.as_str()).cloned(),
            );
        }
        if live.refreshing.contains(c.id.as_str()) {
            return (ConnectionState::Refreshing, None);
        }
        if c.credential_ref.is_some() && c.credential_epoch != Some(restore_epoch) {
            return (
                ConnectionState::ReauthenticationRequired,
                Some("the workspace was restored; sign in again".into()),
            );
        }
        if c.credential_ref.is_none() && p.needs_credential() {
            return (
                ConnectionState::Disconnected,
                live.failures
                    .get(c.id.as_str())
                    .cloned()
                    .or_else(|| c.state_reason.clone()),
            );
        }
        let state = match c.lifecycle.as_str() {
            "Connected" => ConnectionState::Connected,
            "ReauthenticationRequired" => ConnectionState::ReauthenticationRequired,
            "Unavailable" => ConnectionState::Unavailable,
            _ => ConnectionState::Disconnected,
        };
        (state, c.state_reason.clone())
    }

    fn record(
        &self,
        c: &StoredConnection,
        live: &Live,
        restore_epoch: i64,
        now: i64,
        human: Option<&ActorId>,
    ) -> Result<ConnectionRecord, StorageError> {
        let (state, reason) = self.effective(c, live, restore_epoch, now);
        let item = match state {
            ConnectionState::Disabled => ItemState::Disabled,
            ConnectionState::Connected => match c.healthy {
                Some(true) => ItemState::Healthy,
                Some(false) => ItemState::Failed,
                None => ItemState::Enabled,
            },
            ConnectionState::Unavailable | ConnectionState::Refreshing => ItemState::Unavailable,
            ConnectionState::Disconnected
            | ConnectionState::Authorizing
            | ConnectionState::ReauthenticationRequired => ItemState::Partial,
        };
        let ids: Option<Vec<BoundedText<1, 4096>>> = c.discovered.as_ref().and_then(|d| {
            (d.len() <= 128)
                .then(|| d.iter().filter_map(|m| text(&m.id)).collect::<Vec<_>>())
                .filter(|v| v.len() == d.len())
        });
        // The challenge is shown only to the human who began the sign-in.
        let sign_in = human.and_then(|who| {
            live.flows
                .values()
                .find(|f| {
                    f.active
                        && f.connection_id == c.id
                        && f.expires_at_ms > now
                        && &f.started_by == who
                })
                .map(|f| match &f.kind {
                    FlowKind::Device { code, .. } => SignInChallenge {
                        flow_id: f.id.clone(),
                        method: SignInMethod::DeviceCode,
                        user_code: Some(label(&code.user_code, "code")),
                        verification_url: Some(Url(code.verification_url.clone())),
                        authorize_url: None,
                        expires_at_ms: counter(f.expires_at_ms),
                    },
                    FlowKind::Browser { authorize_url, .. } => SignInChallenge {
                        flow_id: f.id.clone(),
                        method: SignInMethod::AuthorizationCode,
                        user_code: None,
                        verification_url: None,
                        authorize_url: Some(Url(authorize_url.clone())),
                        expires_at_ms: counter(f.expires_at_ms),
                    },
                })
        });
        Ok(ConnectionRecord {
            id: c.id.clone(),
            revision: rev(c.revision),
            name: label(&c.name, "connection"),
            profile: parse_wire::<EndpointKind>(&c.profile)?,
            state: item,
            credential_ref: c.credential_ref.clone(),
            subscription_risk_accepted: risk_effective(c, restore_epoch),
            risk_notice_sha256: c
                .risk
                .as_ref()
                .and_then(|r| Digest::try_from(r.notice_sha256.clone()).ok()),
            endpoint: Some(Url(c.endpoint.clone())),
            lifecycle: Some(parse_wire::<ConnectionLifecycle>(state.as_str())?),
            state_reason: reason.as_deref().and_then(text),
            credential_revision: Some(counter(c.credential_revision)),
            healthy: c.healthy,
            checked_at_ms: c.checked_at_ms.map(counter),
            discovered_at_ms: c.discovered_at_ms.map(counter),
            discovered_model_count: c.discovered.as_ref().map(|d| counter(d.len() as i64)),
            discovered_model_ids: ids.map(BoundedList),
            access_expires_at_ms: c.access_expires_at_ms.map(counter),
            sign_in,
            enabled: Some(c.enabled),
            local_free: Some(c.local_free),
            open_router: constraints_of(c),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Credentials for transports (DW-19) and checks

    // -----------------------------------------------------------------------------------------
    // The host tick: device polling, flow expiry, periodic checks, reconciliation

    // -----------------------------------------------------------------------------------------
    // router.connections

    fn connections_query(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ConnectionsInput,
    ) -> Result<OperationResult, Failure> {
        let rows = all_connections(snapshot.router())?;
        // Cursors are bound to the exact set of (connection, revision) pairs, so any change
        // between pages makes the next page re-snapshot instead of mixing two lists.
        let basis: String = rows
            .iter()
            .map(|c| format!("{}:{};", c.id.as_str(), c.revision))
            .collect();
        let digest = crate::fingerprint::sha256_hex(basis.as_bytes());
        let binding = CursorBinding::new(ctx, "router.connections", &[&digest], 1);
        let restore = snapshot.stamp().restore_epoch;
        let now = self.now();
        let human = (ctx.principal_kind() == PrincipalKind::Human).then(|| ctx.principal());
        let live = self.live();
        let page = cursor::page(
            &binding,
            &i.page,
            |after, n| {
                Ok::<_, StorageError>(
                    rows.iter()
                        .filter(|c| after.is_none_or(|a| c.id.as_str() > a))
                        .take(usize::from(n))
                        .collect::<Vec<_>>(),
                )
            },
            |c| c.id.as_str().to_owned(),
        )?;
        Ok(OperationResult::RouterConnections {
            value: page.try_into_page(|c| self.record(c, &live, restore, now, human))?,
        })
    }

    // -----------------------------------------------------------------------------------------
    // router.configure: decisions

    // -----------------------------------------------------------------------------------------
    // router.configure: preparation (outside the unit of work)
}

// ---------------------------------------------------------------------------------------------
// Ports

impl ConnectionDirectory for ConnectionService {
    fn connections(&self, read: &dyn RouterRead) -> Result<Vec<ConnectionStatus>, StorageError> {
        let rows = all_connections(read)?;
        let restore = self.restore_epoch();
        let now = self.now();
        let live = self.live();
        Ok(rows
            .iter()
            .map(|c| {
                let (state, _) = self.effective(c, &live, restore, now);
                ConnectionStatus {
                    state,
                    ..status_filter(c)
                }
            })
            .collect())
    }
}

impl QueryHandler for ConnectionService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answered = match query.input() {
            OperationInput::Router(ops::Input::Connections(i)) => {
                self.connections_query(snapshot, ctx, i)
            }
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let result = answered.map_err(|f| query_fault(f, CONNECTION, self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for ConnectionService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if self.is_closed() {
            return Err(StorageError::Cancelled);
        }
        let OperationInput::Router(ops::Input::Configure(i)) = command.input() else {
            return Ok(None);
        };
        // Only a human's command reaches the network or the secret store; anything else is
        // refused by `decide` without any effect.
        if ctx.principal_kind() != PrincipalKind::Human {
            return Ok(None);
        }
        // A retried request key replays its receipt: no second sign-in, exchange or check.
        if env
            .read(|s| s.receipts().lookup(command.identity()))?
            .is_some()
        {
            return Ok(None);
        }
        Ok(self
            .prepare_action(ctx, &i.action)?
            .map(|p| Prepared::new(Arc::new(Mutex::new(Some(p))))))
    }

    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        let Some(slot) = prepared.and_then(|p| p.get::<Arc<Mutex<Option<Prep>>>>()) else {
            return;
        };
        let prep = slot.locked().take();
        if let Some(prep) = prep {
            self.settle_prep(&prep, reply);
        }
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if self.is_closed() {
            return Err(HandlerFailure::Storage(StorageError::Cancelled));
        }
        if let OperationInput::Router(ops::Input::Configure(input)) = command.input()
            && super::embedding_spaces::EmbeddingSpaceService::owns(&input.action)
        {
            return match self.embedding_spaces.get() {
                Some(service) => service.decide(ctx, command, tx),
                None => decided(
                    tx,
                    Err(refuse(
                        DomainErrorCode::UnsupportedCapability,
                        "embedding_configuration",
                        None,
                    )),
                ),
            };
        }
        let outcome = match command.input() {
            OperationInput::Router(ops::Input::Configure(i)) => {
                let slot = command.prepared::<Arc<Mutex<Option<Prep>>>>();
                let guard = slot.map(|s| s.locked());
                let prep = guard.as_ref().and_then(|g| g.as_ref());
                self.decide_action(tx, ctx, &i.action, prep)
            }
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        decided(tx, outcome)
    }
}
