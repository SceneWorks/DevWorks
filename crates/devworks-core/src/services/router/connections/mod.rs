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
pub mod auth;
pub mod discovery;
pub mod profiles;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, router as ops};
use crate::ports::http::{Entropy, HttpTransport, base64url};
use crate::ports::router::{ConnectionDirectory, ConnectionState, ConnectionStatus};
use crate::ports::secrets::{
    Secret, SecretError, SecretKey, SecretRef, SecretResolver, SecretStore, SecretUnavailable,
    SecretValue, reference_handle,
};
use crate::ports::storage::{
    ApplicationStore, NewEvent, ReadOptions, ReadSnapshot, RouterRead, StorageError,
    StoredConnection, StoredDecisionModel, StoredRiskAcceptance, StoredRouteModel, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    AccountAction, ConnectionLifecycle, ConnectionRecord, DecisionModel, EndpointKind, ItemState,
    MutationSummary, RouteModel, RouterConfigAction, SignInChallenge, SignInMethod, TierName,
};
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page, Url};
use crate::protocol::{
    ActorId, CommandReply, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, RetryAdvice,
    RevisionDimension,
};
use crate::services::cursor::CursorBinding;
use crate::services::router::{TIERS, select, tier_members};
use crate::services::sessions::{Failure, Refusal, counter, decided, label, query_fault, rev};
use auth::{AuthFailure, Credential, DeviceCode, Tokens};
use discovery::{Discovery, ListFailure};
use profiles::{AuthEndpoints, Profile, profile};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

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
/// Connected connections rediscover their models this often (reference
/// `ModelListRefreshInterval`).
pub const DISCOVERY_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;
/// An unavailable connection is tested again this often.
pub const UNAVAILABLE_RECHECK_MS: i64 = 60 * 1000;
/// How often the host reconciles orphaned secret handles.
pub const RECONCILE_INTERVAL_MS: i64 = 60 * 60 * 1000;
/// Consecutive refreshes with an unknown outcome before reauthentication is required (a
/// rotated refresh token is never retried without bound).
const AMBIGUOUS_REFRESH_LIMIT: u32 = 2;
/// Rows read per repository page (its maximum).
const PAGE: u16 = 500;

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
    ambiguous: BTreeMap<String, u32>,
    ticking: bool,
    last_reconcile_ms: Option<i64>,
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
fn text(s: &str) -> Option<BoundedText<1, 4096>> {
    let t: String = s.chars().take(4096).collect();
    (!t.is_empty()).then_some(BoundedText(t))
}
fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
fn invalid(kind: &'static str) -> Failure {
    refuse(DomainErrorCode::InvalidArgument, kind, None)
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
        let page = read.connections(after.as_deref(), PAGE)?;
        let full = page.len() == PAGE as usize;
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}
fn all_models(read: &dyn RouterRead) -> Result<Vec<StoredRouteModel>, StorageError> {
    let mut out: Vec<StoredRouteModel> = Vec::new();
    loop {
        let after = out.last().map(|m| (m.position, m.id.as_str().to_owned()));
        let page = read.route_models(after.as_ref().map(|(p, i)| (*p, i.as_str())), PAGE)?;
        let full = page.len() == PAGE as usize;
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
        let page = read.decision_models(after.as_deref(), PAGE)?;
        let full = page.len() == PAGE as usize;
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
/// The secret-free history payload of a connection.
fn connection_payload(c: &StoredConnection) -> Value {
    json!({
        "connectionId": c.id.as_str(),
        "name": c.name,
        "profile": c.profile,
        "endpoint": c.endpoint,
        "credentialRef": c.credential_ref.as_ref().map(Id::as_str),
        "credentialRevision": c.credential_revision,
        "lifecycle": c.lifecycle,
        "subscriptionRiskAccepted": c.risk.is_some(),
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

/// What `prepare` did for one `router.configure` command.
enum Prep {
    /// BeginSignIn: a flow staged (inactive) under this ID, or why it could not start.
    Begin(Result<Id, String>),
    /// CompleteSignIn: the exchanged credential staged under a handle, still pending, or why
    /// the exchange failed.
    Complete {
        flow_id: Id,
        /// The connection's handle when the exchange ran: deleted once the new one is published.
        retire: Option<Id>,
        outcome: Exchange,
    },
    /// Refresh / Inspect: the connection test run against the credential revision it saw.
    Check {
        credential_revision: i64,
        outcome: Result<Discovery, ListFailure>,
    },
    /// The handles `settle` resolves: `retire` are the target connection's current handle
    /// (deleted once nothing references it), `staged` the submitted API key (unstaged once
    /// published).
    Handles { retire: Vec<Id>, staged: Option<Id> },
}
enum Exchange {
    Staged {
        handle: Id,
        account_digest: Option<String>,
        expires_at_ms: i64,
    },
    Pending,
    Failed(DomainErrorCode, String),
}

// ---------------------------------------------------------------------------------------------
// The service

impl ConnectionService {
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
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn read<T>(
        &self,
        work: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), work)
    }

    // -----------------------------------------------------------------------------------------
    // Host intake (native UI of the human principal only)

    /// Writes an API key the user entered into the secret store under a new handle and returns
    /// the handle (`credentialRef`) the user's `router.configure` `Connection` action publishes.
    /// The handle is accepted only from `principal`; an unpublished one is deleted after
    /// [`STAGED_TTL_MS`].
    pub fn stage_api_key(&self, principal: &ActorId, key: Secret) -> Result<Id, SecretError> {
        let trimmed = key.expose().trim();
        if trimmed.is_empty() {
            return Err(SecretError::Failed("empty key".into()));
        }
        self.write_handle(principal, true, &Credential::ApiKey(Secret::new(trimmed)))
    }

    /// Writes `credential` under a new handle, registered as staged first so a concurrent
    /// reconcile never deletes it before it is published.
    fn write_handle(
        &self,
        principal: &ActorId,
        api_key: bool,
        credential: &Credential,
    ) -> Result<Id, SecretError> {
        let handle = self.ids.next_id(IdKind::Credential);
        self.live().staged.insert(
            handle.as_str().to_owned(),
            Staged {
                principal: principal.clone(),
                api_key,
                created_at_ms: self.now(),
            },
        );
        if let Err(e) = self
            .secrets
            .put(&self.key(&handle), &credential.to_secret())
        {
            self.live().staged.remove(handle.as_str());
            return Err(e);
        }
        Ok(handle)
    }
    fn unstage(&self, handle: &Id) {
        self.live().staged.remove(handle.as_str());
    }
    /// Unstages and deletes a handle that will never be published.
    fn discard(&self, handle: &Id) {
        self.unstage(handle);
        self.delete_unreferenced(std::slice::from_ref(handle));
    }

    /// Writes a settings credential the user entered (for example the web search provider's key,
    /// `web.searchCredential`) into the secret store and returns the reference the user's
    /// `settings.save_revision` stores as `{"secretRef": ...}`. The value is stored under the
    /// reference itself; settings references are owned by settings, so connection
    /// reconciliation never deletes them.
    pub fn stage_setting_secret(&self, value: Secret) -> Result<SecretRef, SecretError> {
        let trimmed = value.expose().trim();
        if trimmed.is_empty() {
            return Err(SecretError::Failed("empty value".into()));
        }
        let mut bytes = [0u8; 16];
        self.entropy.fill(&mut bytes);
        let reference = SecretRef::mint(u128::from_be_bytes(bytes));
        bytes.iter_mut().for_each(|b| *b = 0);
        let handle = reference_handle(&reference)
            .ok_or_else(|| SecretError::Failed("unrepresentable reference".into()))?;
        self.secrets
            .put(&self.key(&handle), &Secret::new(trimmed))?;
        Ok(reference)
    }

    /// Attaches the `code#state` value Claude showed to `principal`'s open browser sign-in, for
    /// the `CompleteSignIn` command that follows. It stays in memory only.
    pub fn submit_authorization_code(
        &self,
        principal: &ActorId,
        flow_id: &Id,
        pasted: Secret,
    ) -> Result<(), IntakeError> {
        let mut live = self.live();
        let now = self.now();
        let flow = live
            .flows
            .get_mut(flow_id.as_str())
            .filter(|f| f.active && &f.started_by == principal && f.expires_at_ms > now)
            .ok_or(IntakeError::UnknownFlow)?;
        match &mut flow.kind {
            FlowKind::Browser { pasted: slot, .. } => {
                *slot = Some(pasted);
                Ok(())
            }
            FlowKind::Device { .. } => Err(IntakeError::NotBrowserFlow),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Effective state

    fn effective(
        &self,
        c: &StoredConnection,
        live: &Live,
        restore_epoch: i64,
        now: i64,
    ) -> (ConnectionState, Option<String>) {
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
        })
    }

    // -----------------------------------------------------------------------------------------
    // Credentials for transports (DW-19) and checks

    fn refresh_lock(&self, connection: &Id) -> Arc<Mutex<()>> {
        self.refresh_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(connection.as_str().to_owned())
            .or_default()
            .clone()
    }

    /// The credential for one request on `connection_id`. An OAuth access token near expiry, or
    /// equal to `rejected` (the provider just refused it), is refreshed first: one refresh per
    /// connection at a time, shared by concurrent callers, persisted whatever happens to the
    /// caller. Network failure is an availability error, never a revocation.
    pub fn access(
        &self,
        connection_id: &Id,
        rejected: Option<&Secret>,
    ) -> Result<Access, AccessError> {
        self.access_with(connection_id, rejected, false)
    }

    /// Forces a token refresh (the `Refresh` action); API-key connections only re-read.
    pub fn refresh(&self, connection_id: &Id) -> Result<Access, AccessError> {
        self.access_with(connection_id, None, true)
    }

    fn access_with(
        &self,
        connection_id: &Id,
        rejected: Option<&Secret>,
        force: bool,
    ) -> Result<Access, AccessError> {
        let restore = self.restore_epoch();
        let unavailable = |e: StorageError| AccessError::Unavailable(format!("{e:?}"));
        let c = self
            .read(|s| s.router().connection(connection_id))
            .map_err(unavailable)?
            .ok_or(AccessError::NotFound)?;
        let p = profile(&c.profile).ok_or(AccessError::NotFound)?;
        if p.risk_gated && !risk_effective(&c, restore) {
            return Err(AccessError::Disabled);
        }
        let base = |credential| Access {
            connection_id: c.id.clone(),
            profile: c.profile.clone(),
            endpoint: c.endpoint.clone(),
            credential_revision: c.credential_revision,
            credential,
        };
        let Some(handle) = c.credential_ref.clone() else {
            return if p.needs_credential() {
                Err(AccessError::Disconnected)
            } else {
                Ok(base(AccessCredential::None))
            };
        };
        if c.credential_epoch != Some(restore) {
            return Err(AccessError::ReauthenticationRequired(
                "the workspace was restored; sign in again".into(),
            ));
        }
        if c.lifecycle == "ReauthenticationRequired" {
            return Err(AccessError::ReauthenticationRequired(
                c.state_reason
                    .clone()
                    .unwrap_or_else(|| "sign in again".into()),
            ));
        }
        let credential = self.load(&c, &handle)?;
        match credential {
            Credential::ApiKey(k) => Ok(base(AccessCredential::ApiKey(k))),
            Credential::OAuth(t) => {
                let fresh = t.expires_at_ms > self.now() + auth::REFRESH_SKEW_MS
                    && rejected.is_none_or(|r| r != &t.access);
                if fresh && !force {
                    return Ok(base(AccessCredential::Bearer {
                        token: t.access,
                        account: t.account,
                    }));
                }
                drop(t);
                self.refresh_tokens(&c, rejected, force)
            }
        }
    }

    /// Reads and parses a handle; a missing or unreadable secret requires reauthentication.
    fn load(&self, c: &StoredConnection, handle: &Id) -> Result<Credential, AccessError> {
        match self.secrets.get(&self.key(handle)) {
            Ok(Some(s)) => Credential::parse(&s).ok_or_else(|| {
                self.mark(
                    c,
                    "ReauthenticationRequired",
                    "the stored credential is unreadable",
                );
                AccessError::ReauthenticationRequired("the stored credential is unreadable".into())
            }),
            Ok(None) => {
                self.mark(
                    c,
                    "ReauthenticationRequired",
                    "the stored credential is missing",
                );
                Err(AccessError::ReauthenticationRequired(
                    "the stored credential is missing; sign in or enter the key again".into(),
                ))
            }
            Err(e) => Err(AccessError::Unavailable(e.to_string())),
        }
    }

    /// Records a lifecycle for `c` if its credential is still the one it was decided for.
    fn mark(&self, c: &StoredConnection, lifecycle: &'static str, reason: &str) {
        let (id, seen, reason) = (c.id.clone(), c.credential_revision, reason.to_owned());
        let host = self.store.info().host_actor;
        let _ = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut cur) = tx.router().connection(&id)? else {
                return Ok(());
            };
            if cur.credential_revision != seen || cur.lifecycle == lifecycle {
                return Ok(());
            }
            let expected = cur.revision;
            cur.lifecycle = lifecycle.into();
            cur.state_reason = Some(reason);
            cur.updated_at_ms = tx.now_ms();
            let saved = tx.router().put_connection(&cur, Some(expected))?;
            event(
                tx,
                &host,
                (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                "ConnectionStateChanged",
                connection_payload(&saved),
            )
        });
    }

    fn refresh_tokens(
        &self,
        seen: &StoredConnection,
        rejected: Option<&Secret>,
        force: bool,
    ) -> Result<Access, AccessError> {
        let lock = self.refresh_lock(&seen.id);
        let _serialized = lock.lock().unwrap_or_else(|p| p.into_inner());
        // Another caller may have refreshed while this one waited.
        let c = self
            .read(|s| s.router().connection(&seen.id))
            .map_err(|e| AccessError::Unavailable(format!("{e:?}")))?
            .ok_or(AccessError::NotFound)?;
        let Some(handle) = c.credential_ref.clone() else {
            return Err(AccessError::Disconnected);
        };
        let Credential::OAuth(tokens) = self.load(&c, &handle)? else {
            return Err(AccessError::ReauthenticationRequired(
                "the stored credential is not a subscription token".into(),
            ));
        };
        let base = |token: Secret, account: Option<String>, revision: i64| Access {
            connection_id: c.id.clone(),
            profile: c.profile.clone(),
            endpoint: c.endpoint.clone(),
            credential_revision: revision,
            credential: AccessCredential::Bearer { token, account },
        };
        if c.credential_revision != seen.credential_revision
            && !force
            && tokens.expires_at_ms > self.now() + auth::REFRESH_SKEW_MS
            && rejected.is_none_or(|r| r != &tokens.access)
        {
            return Ok(base(tokens.access, tokens.account, c.credential_revision));
        }
        struct Refreshing<'a>(&'a ConnectionService, String);
        impl Drop for Refreshing<'_> {
            fn drop(&mut self) {
                self.0.live().refreshing.remove(&self.1);
            }
        }
        self.live().refreshing.insert(c.id.as_str().to_owned());
        let _guard = Refreshing(self, c.id.as_str().to_owned());
        let now = self.now();
        let outcome = match c.profile.as_str() {
            "CodexSubscriptionResponses" => {
                auth::codex_refresh(&*self.http, &self.endpoints, &tokens, now)
            }
            _ => auth::claude_refresh(&*self.http, &self.endpoints, &tokens, now),
        };
        match outcome {
            Ok(rotated) => {
                self.live().ambiguous.remove(c.id.as_str());
                let account = rotated.account.clone();
                let access = rotated.access.clone();
                let expires = rotated.expires_at_ms;
                let digest = rotated.account.as_deref().map(profiles::account_digest);
                let host = self.store.info().host_actor;
                let new_handle = self
                    .write_handle(&host, false, &Credential::OAuth(rotated))
                    .map_err(|e| AccessError::Unavailable(e.to_string()))?;
                let published = self.publish_rotation(&c, &new_handle, digest, expires);
                match published {
                    Ok(Some(revision)) => {
                        self.unstage(&new_handle);
                        self.delete_unreferenced(&[handle]);
                        Ok(base(access, account, revision))
                    }
                    Ok(None) => {
                        // The credential changed meanwhile (a sign-out or a new sign-in): the
                        // rotation is not published, and its tokens are not used.
                        self.discard(&new_handle);
                        Err(AccessError::Unavailable(
                            "the connection's credential changed during the refresh".into(),
                        ))
                    }
                    Err(e) => {
                        // Not published: the old refresh token may already be spent, so the
                        // connection needs a new sign-in rather than a repeat of it.
                        self.discard(&new_handle);
                        self.mark(
                            &c,
                            "ReauthenticationRequired",
                            "the refreshed token could not be saved",
                        );
                        Err(AccessError::Unavailable(format!("{e:?}")))
                    }
                }
            }
            Err(AuthFailure::Rejected(status)) => {
                let why = AuthFailure::Rejected(status).reason();
                self.mark(&c, "ReauthenticationRequired", &why);
                Err(AccessError::ReauthenticationRequired(why))
            }
            Err(e @ AuthFailure::Transport(_)) | Err(e @ AuthFailure::Malformed(_)) => {
                // The request may have reached the issuer and rotated the refresh token: allow
                // a bounded number of such outcomes, then require a new sign-in.
                let n = {
                    let mut live = self.live();
                    let n = live.ambiguous.entry(c.id.as_str().to_owned()).or_insert(0);
                    *n += 1;
                    *n
                };
                if n >= AMBIGUOUS_REFRESH_LIMIT {
                    let why = "the token refresh outcome is unknown; sign in again";
                    self.mark(&c, "ReauthenticationRequired", why);
                    return Err(AccessError::ReauthenticationRequired(why.into()));
                }
                Err(AccessError::Unavailable(e.reason()))
            }
            Err(e @ AuthFailure::Status(_)) => Err(AccessError::Unavailable(e.reason())),
        }
    }

    /// Publishes a rotated token (compare-and-set on the credential revision it replaced); the
    /// new revision, or `None` when the credential changed meanwhile.
    fn publish_rotation(
        &self,
        seen: &StoredConnection,
        handle: &Id,
        account_digest: Option<String>,
        expires_at_ms: i64,
    ) -> Result<Option<i64>, StorageError> {
        let (id, old_ref, old_rev) = (
            seen.id.clone(),
            seen.credential_ref.clone(),
            seen.credential_revision,
        );
        let (handle, host, restore) = (
            handle.clone(),
            self.store.info().host_actor,
            self.restore_epoch(),
        );
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut c) = tx.router().connection(&id)? else {
                return Ok(None);
            };
            if c.credential_ref != old_ref || c.credential_revision != old_rev {
                return Ok(None);
            }
            let expected = c.revision;
            c.credential_ref = Some(handle);
            c.credential_revision += 1;
            c.credential_epoch = Some(restore);
            c.lifecycle = "Connected".into();
            c.state_reason = None;
            c.access_expires_at_ms = Some(expires_at_ms);
            if account_digest.is_some() && account_digest != c.account_digest {
                // A refresh never changes the account; if the provider says otherwise, the risk
                // acceptance no longer covers it.
                c.risk = None;
                c.account_digest = account_digest;
            }
            c.updated_at_ms = tx.now_ms();
            let saved = tx.router().put_connection(&c, Some(expected))?;
            event(
                tx,
                &host,
                (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                "CredentialRotated",
                connection_payload(&saved),
            )?;
            Ok(Some(saved.credential_revision))
        })
    }

    /// Deletes each handle that no connection references and no intake has staged.
    fn delete_unreferenced(&self, handles: &[Id]) {
        if handles.is_empty() {
            return;
        }
        let Ok(refs) = self.read(|s| referenced(s.router())) else {
            return;
        };
        for h in handles {
            let staged = self.live().staged.contains_key(h.as_str());
            if !refs.contains(h.as_str()) && !staged {
                // Best effort: a leftover is an orphan the next reconcile removes.
                let _ = self.secrets.delete(&self.key(h));
            }
        }
    }

    /// Deletes every handle of this store's namespace that no connection references and no
    /// intake has staged (expiring staged keys older than [`STAGED_TTL_MS`]). Never reads a value.
    pub fn reconcile(&self) -> Result<usize, SecretError> {
        let now = self.now();
        let listed = self.secrets.list(&self.namespace())?;
        let refs = self
            .read(|s| referenced(s.router()))
            .map_err(|e| SecretError::Failed(format!("{e:?}")))?;
        let mut removed = 0;
        for h in listed {
            // Settings secret references belong to settings, never to a connection.
            if refs.contains(h.as_str()) || h.as_str().starts_with(SETTING_SECRET_PREFIX) {
                continue;
            }
            {
                let mut live = self.live();
                if let Some(s) = live.staged.get(h.as_str()) {
                    if now - s.created_at_ms < STAGED_TTL_MS {
                        continue;
                    }
                    live.staged.remove(h.as_str());
                }
            }
            // Re-read the references: a publish may have committed meanwhile.
            let still = self
                .read(|s| referenced(s.router()))
                .map_err(|e| SecretError::Failed(format!("{e:?}")))?;
            if still.contains(h.as_str()) {
                continue;
            }
            if self.secrets.delete(&self.key(&h))? {
                removed += 1;
            }
        }
        self.live().last_reconcile_ms = Some(now);
        Ok(removed)
    }

    /// The discovery credential of `c` (refreshing an OAuth token when needed).
    fn check(&self, c: &StoredConnection, force_refresh: bool) -> Result<Discovery, ListFailure> {
        let access = if force_refresh {
            self.refresh(&c.id)
        } else {
            self.access(&c.id, None)
        };
        let access = match access {
            Ok(a) => a,
            Err(AccessError::Unavailable(why)) => return Err(ListFailure::Unreachable(why)),
            Err(AccessError::NotFound) => return Err(ListFailure::Malformed("connection")),
            Err(AccessError::Disabled) => {
                return Err(ListFailure::Unreachable(
                    "the subscription risk notice is not accepted".into(),
                ));
            }
            Err(AccessError::Disconnected) => return Err(ListFailure::Unauthorized(401)),
            Err(AccessError::ReauthenticationRequired(_)) => {
                return Err(ListFailure::Unauthorized(401));
            }
        };
        let run = |a: &Access| {
            let auth = match (&a.credential, c.profile.as_str()) {
                (AccessCredential::None, _) => discovery::Auth::None,
                (AccessCredential::ApiKey(k), "AnthropicMessages") => {
                    discovery::Auth::AnthropicKey(k)
                }
                (AccessCredential::ApiKey(k), _) => discovery::Auth::Bearer(k),
                (AccessCredential::Bearer { token, account }, "CodexSubscriptionResponses") => {
                    discovery::Auth::Codex(token, account.as_deref().unwrap_or(""))
                }
                (AccessCredential::Bearer { token, .. }, _) => {
                    discovery::Auth::ClaudeSubscription(token)
                }
            };
            discovery::list(&*self.http, &self.endpoints, &c.profile, &c.endpoint, &auth)
        };
        match run(&access) {
            // The access token may have been revoked before its expiry; one forced refresh
            // decides (reference `CodexModelList`).
            Err(ListFailure::Unauthorized(_))
                if matches!(access.credential, AccessCredential::Bearer { .. }) =>
            {
                let rejected = match &access.credential {
                    AccessCredential::Bearer { token, .. } => token.clone(),
                    _ => unreachable!("matched a bearer credential"),
                };
                match self.access(&c.id, Some(&rejected)) {
                    Ok(again) => run(&again),
                    Err(AccessError::Unavailable(why)) => Err(ListFailure::Unreachable(why)),
                    Err(_) => Err(ListFailure::Unauthorized(401)),
                }
            }
            other => other,
        }
    }

    /// Tests a connection and records the outcome as the host (periodic checks).
    fn check_as_host(&self, c: &StoredConnection) -> Result<(), StorageError> {
        let outcome = self.check(c, false);
        let (id, seen, host) = (
            c.id.clone(),
            c.credential_revision,
            self.store.info().host_actor,
        );
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            apply_check(tx, &host, &id, seen, &outcome).map(|_| ())
        })
    }

    // -----------------------------------------------------------------------------------------
    // The host tick: device polling, flow expiry, periodic checks, reconciliation

    /// One bounded step of background connection work, driven by the host timer: expires
    /// sign-in flows, polls due device codes (finishing an approved sign-in), retests
    /// unavailable connections, rediscovers stale model lists and reconciles orphaned secret
    /// handles hourly. A tick that is still running makes the next one a no-op.
    pub fn tick(&self) {
        {
            let mut live = self.live();
            if live.ticking {
                return;
            }
            live.ticking = true;
        }
        struct Ticking<'a>(&'a ConnectionService);
        impl Drop for Ticking<'_> {
            fn drop(&mut self) {
                self.0.live().ticking = false;
            }
        }
        let _guard = Ticking(self);
        let now = self.now();
        let due: Vec<Id> = {
            let mut live = self.live();
            live.flows.retain(|_, f| f.expires_at_ms > now || f.busy);
            live.flows
                .values_mut()
                .filter(|f| f.active && !f.busy)
                .filter_map(|f| match &f.kind {
                    FlowKind::Device { next_poll_ms, .. } if *next_poll_ms <= now => {
                        f.busy = true;
                        Some(f.id.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        for flow in due {
            self.poll_device(&flow);
        }
        if let Ok(connections) = self.read(|s| all_connections(s.router())) {
            let restore = self.restore_epoch();
            for c in connections {
                let (state, _) = {
                    let live = self.live();
                    self.effective(&c, &live, restore, now)
                };
                let stale = match state {
                    ConnectionState::Connected => {
                        c.discovered_at_ms
                            .is_none_or(|t| now - t >= DISCOVERY_INTERVAL_MS)
                            && c.checked_at_ms
                                .is_none_or(|t| now - t >= UNAVAILABLE_RECHECK_MS)
                    }
                    ConnectionState::Unavailable => c
                        .checked_at_ms
                        .is_none_or(|t| now - t >= UNAVAILABLE_RECHECK_MS),
                    _ => false,
                };
                if stale {
                    let _ = self.check_as_host(&c);
                }
            }
        }
        let reconcile_due = self
            .live()
            .last_reconcile_ms
            .is_none_or(|t| now - t >= RECONCILE_INTERVAL_MS);
        if reconcile_due {
            let _ = self.reconcile();
        }
    }

    /// Polls one device flow (marked busy by the caller) and finishes an approved sign-in.
    fn poll_device(&self, flow_id: &Id) {
        let snapshot = {
            let live = self.live();
            live.flows
                .get(flow_id.as_str())
                .and_then(|f| match &f.kind {
                    FlowKind::Device { code, .. } => Some((code.clone(), f.started_by.clone())),
                    FlowKind::Browser { .. } => None,
                })
        };
        let Some((code, actor)) = snapshot else {
            return;
        };
        let outcome = self.device_step(&actor, &code);
        match outcome {
            Exchange::Pending => {
                let mut live = self.live();
                if let Some(f) = live.flows.get_mut(flow_id.as_str()) {
                    f.busy = false;
                    if let FlowKind::Device { code, next_poll_ms } = &mut f.kind {
                        *next_poll_ms = self.now() + code.interval_ms;
                    }
                }
            }
            Exchange::Failed(_, why) => {
                let mut live = self.live();
                if let Some(f) = live.flows.remove(flow_id.as_str()) {
                    live.failures
                        .insert(f.connection_id.as_str().to_owned(), why);
                }
            }
            Exchange::Staged {
                handle,
                account_digest,
                expires_at_ms,
            } => {
                let flow = self.live().flows.remove(flow_id.as_str());
                // A flow cancelled while it was exchanging (its connection changed) never
                // publishes.
                let Some(flow) = flow.filter(|f| f.active) else {
                    self.discard(&handle);
                    return;
                };
                let publish = PublishTokens {
                    connection_id: flow.connection_id.clone(),
                    profile: flow.profile.clone(),
                    endpoint: flow.endpoint.clone(),
                    credential_revision: flow.credential_revision,
                    handle: handle.clone(),
                    account_digest,
                    expires_at_ms,
                    restore_epoch: self.restore_epoch(),
                };
                let result = run_write(&*self.store, WriteOptions::default(), move |tx| {
                    publish_tokens(tx, &actor, &publish).map(|r| r.map(|(c, old)| (c.id, old)))
                });
                match result {
                    Ok(Ok((id, old))) => {
                        self.unstage(&handle);
                        self.live().failures.remove(id.as_str());
                        self.delete_unreferenced(&old.into_iter().collect::<Vec<_>>());
                        // Discover the models of the newly signed-in account.
                        if let Ok(Some(c)) = self.read(|s| s.router().connection(&id)) {
                            let _ = self.check_as_host(&c);
                        }
                    }
                    Ok(Err(_)) => {
                        self.live().failures.insert(
                            flow.connection_id.as_str().to_owned(),
                            "the connection changed during the sign-in; sign in again".into(),
                        );
                        self.discard(&handle);
                    }
                    Err(_) => self.discard(&handle),
                }
            }
        }
    }

    /// Polls a device code once; on approval exchanges it and stages the tokens.
    fn device_step(&self, principal: &ActorId, code: &DeviceCode) -> Exchange {
        match auth::codex_poll(&*self.http, &self.endpoints, code) {
            Ok(None) => Exchange::Pending,
            // A transient network failure keeps polling until the code expires (reference).
            Err(AuthFailure::Transport(_)) => Exchange::Pending,
            Err(e) => Exchange::Failed(
                DomainErrorCode::CredentialsRequired,
                format!("the device sign-in was denied or expired: {}", e.reason()),
            ),
            Ok(Some((auth_code, verifier))) => {
                match auth::codex_exchange(
                    &*self.http,
                    &self.endpoints,
                    &auth_code,
                    &verifier,
                    self.now(),
                ) {
                    Ok(tokens) => self.stage_tokens(principal, tokens),
                    Err(e) => Exchange::Failed(DomainErrorCode::ExternalFailure, e.reason()),
                }
            }
        }
    }

    /// Writes exchanged tokens under a new handle (not yet published).
    fn stage_tokens(&self, principal: &ActorId, tokens: Tokens) -> Exchange {
        let account_digest = tokens.account.as_deref().map(profiles::account_digest);
        let expires_at_ms = tokens.expires_at_ms;
        match self.write_handle(principal, false, &Credential::OAuth(tokens)) {
            Ok(handle) => Exchange::Staged {
                handle,
                account_digest,
                expires_at_ms,
            },
            Err(e) => Exchange::Failed(DomainErrorCode::StorageUnavailable, e.to_string()),
        }
    }

    /// Cancels every open sign-in of `connection` (its credential, profile or endpoint changed,
    /// or it was removed).
    fn cancel_flows(&self, connection: &Id) {
        self.live()
            .flows
            .retain(|_, f| &f.connection_id != connection || f.busy);
        let mut live = self.live();
        for f in live.flows.values_mut() {
            if &f.connection_id == connection {
                // A running exchange finishes, but its publish fails the compare-and-set.
                f.active = false;
            }
        }
    }

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
        let after = match &i.page.cursor {
            None => None,
            Some(c) => Some(binding.decode(&c.0)?),
        };
        let limit = i.page.limit.get() as usize;
        let restore = snapshot.stamp().restore_epoch;
        let now = self.now();
        let human = (ctx.principal_kind() == PrincipalKind::Human).then(|| ctx.principal());
        let live = self.live();
        let mut items = Vec::new();
        let mut more = false;
        for c in rows
            .iter()
            .filter(|c| after.as_deref().is_none_or(|a| c.id.as_str() > a))
        {
            if items.len() == limit {
                more = true;
                break;
            }
            items.push(self.record(c, &live, restore, now, human)?);
        }
        let next_cursor = match (more, items.last()) {
            (true, Some(last)) => Some(BoundedText(binding.encode(last.id.as_str()))),
            _ => None,
        };
        Ok(OperationResult::RouterConnections {
            value: Page {
                items: BoundedList(items),
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // router.configure: decisions

    fn decide_action(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        action: &RouterConfigAction,
        prep: Option<&Prep>,
    ) -> Result<DomainDecision, Failure> {
        // `router.configure` is human-only (HumanConfiguration); the binding of a risk decision
        // to a known human principal is rechecked here, never taken from the request.
        if ctx.principal_kind() != PrincipalKind::Human
            || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "human_configuration",
                None,
            ));
        }
        let actor = ctx.principal().clone();
        match action {
            RouterConfigAction::Connection {
                connection_id,
                expected_revision,
                name,
                profile: kind,
                endpoint,
                credential_ref,
            } => self.configure_connection(
                tx,
                &actor,
                ConnectionEdit {
                    id: connection_id.as_ref(),
                    expected: expected_revision.as_ref().map(|r| r.get()),
                    name: &name.0,
                    profile: &wire(kind),
                    endpoint: &endpoint.0,
                    credential_ref: credential_ref.as_ref(),
                },
            ),
            RouterConfigAction::Model {
                model,
                expected_revision,
            } => configure_model(
                tx,
                &actor,
                model,
                expected_revision.as_ref().map(|r| r.get()),
            ),
            RouterConfigAction::Tier {
                tier,
                expected_revision,
                member_ids,
            } => configure_tier(tx, ctx, tier, expected_revision.get(), &member_ids.0),
            RouterConfigAction::DecisionModel {
                model,
                expected_revision,
            } => configure_decision_model(
                tx,
                &actor,
                model,
                expected_revision.as_ref().map(|r| r.get()),
            ),
            RouterConfigAction::SignIn {
                connection_id,
                action,
                flow_id,
            } => self.sign_in(tx, &actor, connection_id, action, flow_id.as_ref(), prep),
            RouterConfigAction::RemoveConnection {
                connection_id,
                expected_revision,
            } => self.remove_connection(tx, &actor, connection_id, expected_revision.get()),
            RouterConfigAction::SubscriptionRisk {
                connection_id,
                expected_revision,
                notice_sha256,
                accepted,
            } => self.subscription_risk(
                tx,
                &actor,
                connection_id,
                expected_revision.get(),
                &String::from(notice_sha256.clone()),
                *accepted,
            ),
        }
    }

    fn configure_connection(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        e: ConnectionEdit<'_>,
    ) -> Result<DomainDecision, Failure> {
        let p = profile(e.profile).ok_or_else(|| invalid("profile"))?;
        profiles::check_endpoint(p, e.endpoint).map_err(invalid)?;
        let now = tx.now_ms();
        let restore = tx.store().restore_epoch;
        let current = match (e.id, e.expected) {
            (None, Some(_)) => return Err(invalid("expected_revision")),
            (Some(id), Some(expected)) => {
                let c = tx
                    .router()
                    .connection(id)?
                    .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(id)))?;
                if c.revision != expected {
                    return Err(Failure::Storage(StorageError::conflict(
                        RevisionDimension::Configuration,
                        CONNECTION,
                        id.clone(),
                        rev(expected),
                        rev(c.revision),
                    )));
                }
                Some(c)
            }
            (Some(id), None) => {
                if tx.router().connection(id)?.is_some() {
                    return Err(refuse(DomainErrorCode::Conflict, CONNECTION, Some(id)));
                }
                None
            }
            (None, None) => None,
        };
        let id = match (&current, e.id) {
            (Some(c), _) => c.id.clone(),
            (None, Some(id)) => id.clone(),
            (None, None) => tx.new_id(IdKind::Connection),
        };
        // A staged API key, only from the principal who staged it, only for an API-key profile.
        let staged = match e.credential_ref {
            Some(r) if current.as_ref().and_then(|c| c.credential_ref.as_ref()) == Some(r) => None,
            Some(r) => {
                if p.oauth() {
                    // Subscription credentials come only from their own sign-in.
                    return Err(refuse(
                        DomainErrorCode::InvalidArgument,
                        CREDENTIAL,
                        Some(r),
                    ));
                }
                let ok = self
                    .live()
                    .staged
                    .get(r.as_str())
                    .is_some_and(|s| s.api_key && &s.principal == actor);
                if !ok {
                    return Err(refuse(DomainErrorCode::NotFound, CREDENTIAL, Some(r)));
                }
                Some(r.clone())
            }
            None => None,
        };
        let mut next = match &current {
            Some(c) => c.clone(),
            None => StoredConnection {
                id: id.clone(),
                name: e.name.to_owned(),
                profile: p.name.to_owned(),
                endpoint: e.endpoint.to_owned(),
                credential_ref: None,
                credential_revision: 0,
                credential_epoch: None,
                lifecycle: without_credential(p).into(),
                state_reason: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: None,
                discovered_at_ms: None,
                healthy: None,
                checked_at_ms: None,
                revision: 1,
                created_at_ms: now,
                updated_at_ms: now,
            },
        };
        let profile_changed = current.as_ref().is_some_and(|c| c.profile != p.name);
        let endpoint_changed = current.as_ref().is_some_and(|c| c.endpoint != e.endpoint);
        let origin_changed = current
            .as_ref()
            .is_some_and(|c| profiles::origin(&c.endpoint) != profiles::origin(e.endpoint));
        if profile_changed && bound_models(tx.router(), &id)? {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "connection_in_use",
                Some(&id),
            ));
        }
        next.name = e.name.to_owned();
        next.profile = p.name.to_owned();
        next.endpoint = e.endpoint.to_owned();
        if profile_changed || endpoint_changed {
            // Another endpoint lists other models and answers for itself.
            next.discovered = None;
            next.discovered_at_ms = None;
            next.healthy = None;
            next.checked_at_ms = None;
            next.risk = None;
        }
        if profile_changed {
            next.account_digest = None;
        }
        let mut credential_changed = false;
        if let Some(r) = &staged {
            next.credential_ref = Some(r.clone());
            next.credential_revision += 1;
            next.credential_epoch = Some(restore);
            next.lifecycle = "Connected".into();
            next.state_reason = None;
            next.access_expires_at_ms = None;
            next.healthy = None;
            credential_changed = true;
        } else if (profile_changed || origin_changed) && next.credential_ref.is_some() {
            // Never forward one profile's or origin's credential to another.
            next.credential_ref = None;
            next.credential_revision += 1;
            next.credential_epoch = None;
            next.lifecycle = without_credential(p).into();
            next.state_reason = None;
            next.access_expires_at_ms = None;
            credential_changed = true;
        } else if profile_changed {
            next.lifecycle = without_credential(p).into();
        }
        if let Some(c) = &current {
            let same = c.name == next.name
                && c.profile == next.profile
                && c.endpoint == next.endpoint
                && c.credential_ref == next.credential_ref;
            if same {
                return Ok(unchanged(&c.id, c.revision));
            }
        }
        next.updated_at_ms = now;
        let saved = tx
            .router()
            .put_connection(&next, current.as_ref().map(|c| c.revision))?;
        if credential_changed || profile_changed || endpoint_changed {
            self.cancel_flows(&saved.id);
            clear_profile_cooldowns(tx, &saved)?;
        }
        event(
            tx,
            actor,
            (CONNECTION_AGGREGATE, &saved.id, saved.revision),
            if current.is_some() {
                "ConnectionConfigured"
            } else {
                "ConnectionCreated"
            },
            connection_payload(&saved),
        )?;
        Ok(applied(&saved.id, saved.revision))
    }

    fn sign_in(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        connection_id: &Id,
        action: &AccountAction,
        flow_id: Option<&Id>,
        prep: Option<&Prep>,
    ) -> Result<DomainDecision, Failure> {
        let c = tx
            .router()
            .connection(connection_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(connection_id)))?;
        let p = profile(&c.profile).ok_or_else(|| invalid("profile"))?;
        let restore = tx.store().restore_epoch;
        match action {
            AccountAction::BeginSignIn => {
                if !p.oauth() {
                    return Err(refuse(
                        DomainErrorCode::UnsupportedCapability,
                        "sign_in",
                        Some(&c.id),
                    ));
                }
                if p.risk_gated && !risk_effective(&c, restore) {
                    return Err(refuse(
                        DomainErrorCode::PermissionDenied,
                        "subscription_risk",
                        Some(&c.id),
                    ));
                }
                match prep {
                    Some(Prep::Begin(Ok(flow))) => {
                        let mut live = self.live();
                        let f = live
                            .flows
                            .get_mut(flow.as_str())
                            .filter(|f| f.connection_id == c.id && !f.decided)
                            .ok_or_else(|| refuse(DomainErrorCode::Conflict, FLOW, Some(flow)))?;
                        if f.credential_revision != c.credential_revision
                            || f.profile != c.profile
                            || f.endpoint != c.endpoint
                        {
                            return Err(refuse(
                                DomainErrorCode::SourceChanged,
                                CONNECTION,
                                Some(&c.id),
                            ));
                        }
                        f.decided = true;
                        Ok(applied(&c.id, c.revision))
                    }
                    Some(Prep::Begin(Err(_))) => Err(Refusal::new(
                        DomainErrorCode::ExternalFailure,
                        "sign_in",
                        Some(c.id.clone()),
                    )
                    .retry(RetryAdvice::RefreshAndNewRequest)
                    .into()),
                    _ => Err(refuse(DomainErrorCode::Conflict, FLOW, None)),
                }
            }
            AccountAction::CompleteSignIn => {
                let flow = flow_id.ok_or_else(|| invalid("flow_id"))?;
                if p.risk_gated && !risk_effective(&c, restore) {
                    return Err(refuse(
                        DomainErrorCode::PermissionDenied,
                        "subscription_risk",
                        Some(&c.id),
                    ));
                }
                match prep {
                    Some(Prep::Complete {
                        flow_id: prepared,
                        outcome,
                        ..
                    }) if prepared == flow => match outcome {
                        Exchange::Pending => Ok(unchanged(&c.id, c.revision)),
                        Exchange::Failed(code, _) => {
                            Err(Refusal::new(*code, "sign_in", Some(c.id.clone()))
                                .retry(RetryAdvice::RefreshAndNewRequest)
                                .into())
                        }
                        Exchange::Staged {
                            handle,
                            account_digest,
                            expires_at_ms,
                        } => {
                            let (profile_name, endpoint, revision) = {
                                let live = self.live();
                                let f = live.flows.get(flow.as_str()).ok_or_else(|| {
                                    refuse(DomainErrorCode::NotFound, FLOW, Some(flow))
                                })?;
                                (f.profile.clone(), f.endpoint.clone(), f.credential_revision)
                            };
                            let publish = PublishTokens {
                                connection_id: c.id.clone(),
                                profile: profile_name,
                                endpoint,
                                credential_revision: revision,
                                handle: handle.clone(),
                                account_digest: account_digest.clone(),
                                expires_at_ms: *expires_at_ms,
                                restore_epoch: restore,
                            };
                            match publish_tokens(tx, actor, &publish)? {
                                Ok((saved, _)) => Ok(applied(&saved.id, saved.revision)),
                                Err(kind) => {
                                    Err(refuse(DomainErrorCode::SourceChanged, kind, Some(&c.id)))
                                }
                            }
                        }
                    },
                    _ => Err(refuse(DomainErrorCode::NotFound, FLOW, Some(flow))),
                }
            }
            AccountAction::Refresh | AccountAction::Inspect
                if p.risk_gated && !risk_effective(&c, restore) =>
            {
                Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "subscription_risk",
                    Some(&c.id),
                ))
            }
            AccountAction::Refresh | AccountAction::Inspect => match prep {
                Some(Prep::Check {
                    credential_revision,
                    outcome,
                }) => {
                    // A recorded test (passed or failed: its result is new information) is
                    // applied; one whose credential changed while it ran records nothing.
                    let recorded = apply_check(tx, actor, &c.id, *credential_revision, outcome)?;
                    let after = tx
                        .router()
                        .connection(&c.id)?
                        .map_or(c.revision, |n| n.revision);
                    Ok(if recorded {
                        applied(&c.id, after)
                    } else {
                        unchanged(&c.id, after)
                    })
                }
                _ => Err(refuse(DomainErrorCode::Conflict, CONNECTION, Some(&c.id))),
            },
            AccountAction::Disconnect => {
                if c.credential_ref.is_none() {
                    self.cancel_flows(&c.id);
                    return Ok(unchanged(&c.id, c.revision));
                }
                let mut next = c.clone();
                next.credential_ref = None;
                next.credential_revision += 1;
                next.credential_epoch = None;
                next.lifecycle = without_credential(p).into();
                next.state_reason = None;
                next.access_expires_at_ms = None;
                next.healthy = None;
                next.updated_at_ms = tx.now_ms();
                let saved = tx.router().put_connection(&next, Some(c.revision))?;
                self.cancel_flows(&saved.id);
                clear_profile_cooldowns(tx, &saved)?;
                event(
                    tx,
                    actor,
                    (CONNECTION_AGGREGATE, &saved.id, saved.revision),
                    "ConnectionDisconnected",
                    connection_payload(&saved),
                )?;
                Ok(applied(&saved.id, saved.revision))
            }
        }
    }

    fn remove_connection(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        id: &Id,
        expected: i64,
    ) -> Result<DomainDecision, Failure> {
        let c = tx
            .router()
            .connection(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(id)))?;
        if c.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                CONNECTION,
                id.clone(),
                rev(expected),
                rev(c.revision),
            )));
        }
        if bound_models(tx.router(), id)? {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "connection_in_use",
                Some(id),
            ));
        }
        tx.router().delete_connection(id, expected)?;
        self.cancel_flows(id);
        event(
            tx,
            actor,
            (CONNECTION_AGGREGATE, id, expected + 1),
            "ConnectionRemoved",
            connection_payload(&c),
        )?;
        Ok(applied(id, expected + 1))
    }

    fn subscription_risk(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        id: &Id,
        expected: i64,
        notice: &str,
        accepted: bool,
    ) -> Result<DomainDecision, Failure> {
        let c = tx
            .router()
            .connection(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, CONNECTION, Some(id)))?;
        let p = profile(&c.profile).ok_or_else(|| invalid("profile"))?;
        if !p.risk_gated {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "subscription_risk",
                Some(id),
            ));
        }
        if c.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                CONNECTION,
                id.clone(),
                rev(expected),
                rev(c.revision),
            )));
        }
        // Only the exact current notice can be accepted.
        if accepted && notice != profiles::risk_notice_sha256() {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "risk_notice",
                Some(id.clone()),
            )
            .retry(RetryAdvice::RefreshAndNewRequest)
            .into());
        }
        let restore = tx.store().restore_epoch;
        let effective = risk_effective(&c, restore);
        if accepted == effective && (accepted || c.risk.is_none()) {
            return Ok(unchanged(id, c.revision));
        }
        let now = tx.now_ms();
        let mut next = c.clone();
        next.risk = accepted.then(|| StoredRiskAcceptance {
            notice_sha256: profiles::risk_notice_sha256(),
            binding: profiles::risk_binding(&c.profile, &c.endpoint, c.account_digest.as_deref()),
            restore_epoch: restore,
            accepted_by: actor.0.as_str().to_owned(),
            accepted_at_ms: now,
        });
        next.updated_at_ms = now;
        let saved = tx.router().put_connection(&next, Some(c.revision))?;
        if !accepted {
            self.cancel_flows(id);
        }
        event(
            tx,
            actor,
            (CONNECTION_AGGREGATE, id, saved.revision),
            if accepted {
                "SubscriptionRiskAccepted"
            } else {
                "SubscriptionRiskRevoked"
            },
            json!({
                "connectionId": id.as_str(),
                "noticeSha256": profiles::risk_notice_sha256(),
                "accepted": accepted,
            }),
        )?;
        Ok(applied(id, saved.revision))
    }

    // -----------------------------------------------------------------------------------------
    // router.configure: preparation (outside the unit of work)

    fn prepare_action(
        &self,
        ctx: &InvocationContext,
        action: &RouterConfigAction,
    ) -> Result<Option<Prep>, StorageError> {
        match action {
            RouterConfigAction::Connection {
                connection_id,
                credential_ref,
                ..
            } => {
                let retire = match connection_id {
                    Some(id) => self
                        .read(|s| s.router().connection(id))?
                        .and_then(|c| c.credential_ref)
                        .into_iter()
                        .collect(),
                    None => Vec::new(),
                };
                Ok(Some(Prep::Handles {
                    retire,
                    staged: credential_ref.clone(),
                }))
            }
            RouterConfigAction::RemoveConnection { connection_id, .. } => {
                let retire = self
                    .read(|s| s.router().connection(connection_id))?
                    .and_then(|c| c.credential_ref)
                    .into_iter()
                    .collect();
                Ok(Some(Prep::Handles {
                    retire,
                    staged: None,
                }))
            }
            RouterConfigAction::SignIn {
                connection_id,
                action,
                flow_id,
            } => {
                let Some(c) = self.read(|s| s.router().connection(connection_id))? else {
                    return Ok(None);
                };
                match action {
                    AccountAction::Disconnect => Ok(Some(Prep::Handles {
                        retire: c.credential_ref.into_iter().collect(),
                        staged: None,
                    })),
                    AccountAction::BeginSignIn => Ok(self.begin_flow(ctx, &c).map(Prep::Begin)),
                    AccountAction::CompleteSignIn => Ok(flow_id
                        .as_ref()
                        .and_then(|f| self.complete_flow(ctx, &c, f))),
                    AccountAction::Refresh | AccountAction::Inspect
                        if profile(&c.profile).is_some_and(|p| p.risk_gated)
                            && !risk_effective(&c, self.restore_epoch()) =>
                    {
                        // Refused by `decide`; nothing is sent for a disabled subscription.
                        Ok(None)
                    }
                    AccountAction::Refresh | AccountAction::Inspect => {
                        let force = matches!(action, AccountAction::Refresh)
                            && profile(&c.profile).is_some_and(Profile::oauth);
                        let outcome = self.check(&c, force);
                        // The credential the check ran with (a refresh may have rotated it).
                        let seen = self
                            .read(|s| s.router().connection(&c.id))?
                            .map_or(c.credential_revision, |n| n.credential_revision);
                        Ok(Some(Prep::Check {
                            credential_revision: seen,
                            outcome,
                        }))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    /// Starts a sign-in flow (staged, inactive until its command applies).
    fn begin_flow(
        &self,
        ctx: &InvocationContext,
        c: &StoredConnection,
    ) -> Option<Result<Id, String>> {
        let p = profile(&c.profile)?;
        let now = self.now();
        let kind = match p.auth {
            profiles::Auth::DeviceCode => match auth::codex_start(&*self.http, &self.endpoints) {
                Ok(code) => FlowKind::Device {
                    next_poll_ms: now + code.interval_ms,
                    code,
                },
                Err(e) => return Some(Err(e.reason())),
            },
            profiles::Auth::AuthorizationCode => {
                let verifier = self.random();
                let state = self.random();
                let authorize_url = auth::claude_authorize_url(&self.endpoints, &verifier, &state);
                FlowKind::Browser {
                    verifier,
                    state,
                    authorize_url,
                    pasted: None,
                }
            }
            profiles::Auth::ApiKey { .. } => return None,
        };
        let id = self.ids.next_id(IdKind::SignInFlow);
        let lifetime = match kind {
            FlowKind::Device { .. } => auth::DEVICE_LIFETIME_MS,
            FlowKind::Browser { .. } => auth::BROWSER_LIFETIME_MS,
        };
        let mut live = self.live();
        live.failures.remove(c.id.as_str());
        live.flows.insert(
            id.as_str().to_owned(),
            Flow {
                id: id.clone(),
                connection_id: c.id.clone(),
                profile: c.profile.clone(),
                endpoint: c.endpoint.clone(),
                credential_revision: c.credential_revision,
                started_by: ctx.principal().clone(),
                kind,
                expires_at_ms: now + lifetime,
                decided: false,
                active: false,
                busy: false,
            },
        );
        Some(Ok(id))
    }

    /// Takes an open flow of the caller for this connection and finishes it if it can.
    fn complete_flow(
        &self,
        ctx: &InvocationContext,
        c: &StoredConnection,
        flow_id: &Id,
    ) -> Option<Prep> {
        let now = self.now();
        let taken = {
            let mut live = self.live();
            let f = live.flows.get_mut(flow_id.as_str()).filter(|f| {
                f.active
                    && !f.busy
                    && f.connection_id == c.id
                    && &f.started_by == ctx.principal()
                    && f.expires_at_ms > now
            })?;
            f.busy = true;
            match &mut f.kind {
                FlowKind::Device { code, .. } => Ok(code.clone()),
                FlowKind::Browser {
                    verifier,
                    state,
                    pasted,
                    ..
                } => Err((verifier.clone(), state.clone(), pasted.take())),
            }
        };
        let device = taken.is_ok();
        let outcome = match taken {
            Ok(code) => self.device_step(ctx.principal(), &code),
            Err((_, _, None)) => Exchange::Failed(
                DomainErrorCode::InvalidArgument,
                "no authorization code was entered".into(),
            ),
            Err((verifier, state, Some(pasted))) => {
                match auth::claude_pasted_code(&pasted, &state) {
                    Err(why) => Exchange::Failed(DomainErrorCode::InvalidArgument, why.into()),
                    Ok(code) => match auth::claude_exchange(
                        &*self.http,
                        &self.endpoints,
                        &code,
                        &state,
                        &verifier,
                        now,
                    ) {
                        Ok(tokens) => self.stage_tokens(ctx.principal(), tokens),
                        Err(e @ AuthFailure::Rejected(_)) => {
                            Exchange::Failed(DomainErrorCode::CredentialsRequired, e.reason())
                        }
                        Err(e) => Exchange::Failed(DomainErrorCode::ExternalFailure, e.reason()),
                    },
                }
            }
        };
        let mut live = self.live();
        if let Exchange::Failed(_, why) = &outcome {
            live.failures.insert(c.id.as_str().to_owned(), why.clone());
        }
        if device && matches!(outcome, Exchange::Failed(..)) {
            // A denied or expired device code is finished; a wrong browser paste may be retried.
            live.flows.remove(flow_id.as_str());
        } else if let Some(f) = live.flows.get_mut(flow_id.as_str()) {
            f.busy = false;
            if let FlowKind::Device { code, next_poll_ms } = &mut f.kind {
                *next_poll_ms = now + code.interval_ms;
            }
        }
        Some(Prep::Complete {
            flow_id: flow_id.clone(),
            retire: c.credential_ref.clone(),
            outcome,
        })
    }

    /// Resolves what the unit of work did not commit. Idempotent and decided from durable
    /// state (what a connection references now), so a refusal, a replay, a rollback or a race
    /// with recovery never deletes a published credential.
    fn settle_prep(&self, prep: &Prep, reply: Option<&DomainCommandReply>) {
        let applied_now = matches!(
            reply,
            Some(CommandReply::Recorded { receipt, .. })
                if matches!(receipt.decision, DomainDecision::Applied { .. })
        );
        let published = |h: &Id| {
            self.read(|s| referenced(s.router()))
                .is_ok_and(|r| r.contains(h.as_str()))
        };
        match prep {
            Prep::Begin(Ok(flow)) => {
                let mut live = self.live();
                let keep = applied_now && live.flows.get(flow.as_str()).is_some_and(|f| f.decided);
                if keep {
                    // The new sign-in replaces any earlier one of the same connection.
                    let connection = live
                        .flows
                        .get(flow.as_str())
                        .map(|f| f.connection_id.clone());
                    live.flows.retain(|k, f| {
                        k == flow.as_str()
                            || f.busy
                            || Some(&f.connection_id) != connection.as_ref()
                    });
                    if let Some(f) = live.flows.get_mut(flow.as_str()) {
                        f.active = true;
                    }
                } else {
                    live.flows.remove(flow.as_str());
                }
            }
            Prep::Begin(Err(_)) | Prep::Check { .. } => {}
            Prep::Complete {
                flow_id,
                retire,
                outcome,
            } => {
                if let Exchange::Staged { handle, .. } = outcome {
                    if published(handle) {
                        self.unstage(handle);
                        {
                            let mut live = self.live();
                            if let Some(f) = live.flows.remove(flow_id.as_str()) {
                                live.failures.remove(f.connection_id.as_str());
                            }
                        }
                        self.delete_unreferenced(&retire.iter().cloned().collect::<Vec<_>>());
                    } else {
                        // An exchanged code is single-use: unpublished tokens are discarded.
                        self.discard(handle);
                    }
                }
            }
            Prep::Handles { retire, staged } => {
                if let Some(s) = staged
                    && published(s)
                {
                    self.unstage(s);
                }
                self.delete_unreferenced(retire);
            }
        }
    }
}

/// The fields of a `Connection` action.
struct ConnectionEdit<'a> {
    id: Option<&'a Id>,
    expected: Option<i64>,
    name: &'a str,
    profile: &'a str,
    endpoint: &'a str,
    credential_ref: Option<&'a Id>,
}

/// Exchanged tokens to publish for a sign-in.
struct PublishTokens {
    connection_id: Id,
    profile: String,
    endpoint: String,
    /// The credential revision the sign-in began at (compare-and-set).
    credential_revision: i64,
    handle: Id,
    account_digest: Option<String>,
    expires_at_ms: i64,
    restore_epoch: i64,
}

/// Publishes a sign-in's tokens: the connection still has the profile, endpoint and credential
/// revision the sign-in began with. Returns the saved connection and the handle it replaced,
/// or the refusal kind when the connection changed meanwhile.
fn publish_tokens(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    t: &PublishTokens,
) -> Result<Result<(StoredConnection, Option<Id>), &'static str>, StorageError> {
    let Some(c) = tx.router().connection(&t.connection_id)? else {
        return Ok(Err(CONNECTION));
    };
    if c.profile != t.profile
        || c.endpoint != t.endpoint
        || c.credential_revision != t.credential_revision
    {
        return Ok(Err(CONNECTION));
    }
    let mut next = c.clone();
    let old = c.credential_ref.clone();
    next.credential_ref = Some(t.handle.clone());
    next.credential_revision += 1;
    next.credential_epoch = Some(t.restore_epoch);
    next.lifecycle = "Connected".into();
    next.state_reason = None;
    next.access_expires_at_ms = Some(t.expires_at_ms);
    match (&c.account_digest, &t.account_digest) {
        // The first account binds a risk acceptance given before the first sign-in.
        (None, Some(new)) => {
            if let Some(r) = &mut next.risk
                && r.binding == profiles::risk_binding(&c.profile, &c.endpoint, None)
            {
                r.binding = profiles::risk_binding(&c.profile, &c.endpoint, Some(new));
            }
            next.account_digest = Some(new.clone());
        }
        // Another account: the acceptance (and the last model list) belonged to the old one.
        (Some(old_account), Some(new)) if old_account != new => {
            next.risk = None;
            next.account_digest = Some(new.clone());
            next.discovered = None;
            next.discovered_at_ms = None;
        }
        _ => {}
    }
    next.healthy = None;
    next.updated_at_ms = tx.now_ms();
    let saved = tx.router().put_connection(&next, Some(c.revision))?;
    // A new sign-in starts clean: earlier cooldowns of the profile's models belonged to the
    // previous account (reference `SaveLoginAsync`).
    clear_profile_cooldowns(tx, &saved)?;
    event(
        tx,
        actor,
        (CONNECTION_AGGREGATE, &saved.id, saved.revision),
        "ConnectionSignedIn",
        connection_payload(&saved),
    )?;
    Ok(Ok((saved, old)))
}

/// Ends the cooldowns of the catalog models `c` may serve (a credential or endpoint change).
fn clear_profile_cooldowns(
    tx: &mut dyn WriteTransaction,
    c: &StoredConnection,
) -> Result<(), StorageError> {
    let models = all_models(tx.router())?;
    for m in models.iter().filter(|m| {
        m.endpoint_profile == c.profile && m.connection_id.as_ref().is_none_or(|b| b == &c.id)
    }) {
        tx.router().clear_cooldown(&m.id)?;
    }
    Ok(())
}

/// Records a connection test against the credential revision it ran with; a check whose
/// credential changed meanwhile records nothing. Returns whether it was recorded.
fn apply_check(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    id: &Id,
    seen_credential: i64,
    outcome: &Result<Discovery, ListFailure>,
) -> Result<bool, StorageError> {
    let Some(c) = tx.router().connection(id)? else {
        return Ok(false);
    };
    if c.credential_revision != seen_credential {
        return Ok(false);
    }
    let now = tx.now_ms();
    let mut next = c.clone();
    next.checked_at_ms = Some(now);
    match outcome {
        Ok(d) => {
            let mut models = d.models.clone();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            models.dedup_by(|a, b| a.id == b.id);
            next.discovered = Some(models);
            next.discovered_at_ms = Some(now);
            next.healthy = Some(true);
            next.state_reason = None;
            if next.credential_ref.is_some()
                || profile(&c.profile).is_some_and(|p| !p.needs_credential())
            {
                next.lifecycle = "Connected".into();
            }
        }
        Err(ListFailure::Unauthorized(status)) => {
            next.healthy = Some(false);
            next.state_reason = Some(ListFailure::Unauthorized(*status).reason());
            if next.credential_ref.is_some() {
                next.lifecycle = "ReauthenticationRequired".into();
            }
        }
        Err(e) => {
            // An availability event, not a revocation: the last good list is kept.
            next.healthy = Some(false);
            next.state_reason = Some(e.reason());
            if next.lifecycle == "Connected" {
                next.lifecycle = "Unavailable".into();
            }
        }
    }
    next.updated_at_ms = now;
    let saved = tx.router().put_connection(&next, Some(c.revision))?;
    if let Ok(d) = outcome {
        apply_discovery(tx, &saved, d)?;
    }
    event(
        tx,
        actor,
        (CONNECTION_AGGREGATE, &saved.id, saved.revision),
        "ConnectionChecked",
        json!({
            "connectionId": saved.id.as_str(),
            "healthy": saved.healthy,
            "lifecycle": saved.lifecycle,
            "discoveredModelCount": saved.discovered.as_ref().map(Vec::len),
        }),
    )?;
    Ok(true)
}

/// Applies provider-reported facts to the catalog models a connection serves: an unknown
/// context window from the model list; for OpenRouter, the reported tools and image support
/// and, when none is declared, the supported efforts (reference `UpdateOpenRouterModelsAsync`).
fn apply_discovery(
    tx: &mut dyn WriteTransaction,
    c: &StoredConnection,
    d: &Discovery,
) -> Result<(), StorageError> {
    let listed: BTreeSet<String> = d.models.iter().map(|m| m.id.clone()).collect();
    let models = all_models(tx.router())?;
    for m in models.iter().filter(|m| {
        m.endpoint_profile == c.profile && m.connection_id.as_ref().is_none_or(|b| b == &c.id)
    }) {
        if m.upstream_model_id == select::OPEN_ROUTER_AUTO_UPSTREAM
            || !select::lists(&c.profile, &listed, &m.upstream_model_id)
        {
            continue;
        }
        let found = d.models.iter().find(|x| {
            select::lists(
                &c.profile,
                &BTreeSet::from([x.id.clone()]),
                &m.upstream_model_id,
            )
        });
        let mut next = m.clone();
        if next.context_tokens.is_none() {
            next.context_tokens = found.and_then(|x| x.context_tokens);
        }
        if let Some((_, r)) = d.reported.iter().find(|(id, _)| id == &m.upstream_model_id) {
            if let Some(t) = r.tools {
                next.tools = t;
            }
            if let Some(v) = r.vision {
                next.image_input = v;
            }
            if next.efforts.is_empty() && !r.efforts.is_empty() {
                next.efforts = r.efforts.clone();
            }
        }
        if next != *m {
            next.updated_at_ms = tx.now_ms();
            tx.router().put_route_model(&next, Some(m.revision))?;
        }
    }
    Ok(())
}

/// `Model`: a catalog model edit (create, or compare-and-set update).
fn configure_model(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    model: &RouteModel,
    expected: Option<i64>,
) -> Result<DomainDecision, Failure> {
    let profile_name = wire(&model.endpoint_profile);
    if profile_name == "SystemOne" {
        return Err(invalid("endpoint_profile"));
    }
    let efforts: Vec<String> = model.efforts.0.iter().map(wire).collect();
    if efforts.iter().any(|e| e == select::DEFAULT_EFFORT) {
        return Err(invalid("efforts"));
    }
    let unique: BTreeSet<&String> = efforts.iter().collect();
    if unique.len() != efforts.len() {
        return Err(invalid("efforts"));
    }
    if tx.router().decision_model(&model.id)?.is_some() {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "decision_model",
            Some(&model.id),
        ));
    }
    if let Some(binding) = &model.connection_id {
        match tx.router().connection(binding)? {
            None => return Err(refuse(DomainErrorCode::NotFound, CONNECTION, Some(binding))),
            Some(c) if c.profile != profile_name => {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "connection_profile",
                    Some(binding),
                ));
            }
            Some(_) => {}
        }
    }
    let context = match &model.context_tokens {
        None => None,
        Some(n) => Some(n.0.parse::<i64>().map_err(|_| invalid("context_tokens"))?),
    };
    // Tier membership has one home, the routing settings (`Tier`): it cannot change here.
    let members = tier_members(tx.configuration())?;
    let current_tiers: Vec<String> = TIERS
        .iter()
        .filter(|(t, _)| members.get(t).is_some_and(|ids| ids.contains(&model.id)))
        .map(|(t, _)| (*t).to_owned())
        .collect();
    let mut asked: Vec<String> = model.tier_memberships.0.iter().map(wire).collect();
    asked.sort_by_key(|t| TIERS.iter().position(|(n, _)| n == t));
    asked.dedup();
    if asked != current_tiers {
        return Err(invalid("tier_memberships"));
    }
    let current = tx.router().route_model(&model.id)?;
    let now = tx.now_ms();
    let position = match (&current, expected) {
        (Some(_), None) => {
            return Err(refuse(
                DomainErrorCode::Conflict,
                MODEL_AGGREGATE,
                Some(&model.id),
            ));
        }
        (None, Some(_)) => {
            return Err(refuse(
                DomainErrorCode::NotFound,
                MODEL_AGGREGATE,
                Some(&model.id),
            ));
        }
        (Some(c), Some(e)) => {
            if c.revision != e {
                return Err(Failure::Storage(StorageError::conflict(
                    RevisionDimension::Configuration,
                    MODEL_AGGREGATE,
                    model.id.clone(),
                    rev(e),
                    rev(c.revision),
                )));
            }
            c.position
        }
        (None, None) => all_models(tx.router())?
            .iter()
            .map(|m| m.position)
            .max()
            .unwrap_or(0)
            .saturating_add(10),
    };
    let next = StoredRouteModel {
        id: model.id.clone(),
        display_name: model.display_name.0.clone(),
        endpoint_profile: profile_name,
        upstream_model_id: model.upstream_model_id.0.clone(),
        enabled: model.enabled,
        efforts,
        image_input: model.image_input,
        tools: model.tools,
        context_tokens: context,
        connection_id: model.connection_id.clone(),
        position,
        revision: current.as_ref().map_or(1, |c| c.revision),
        updated_at_ms: now,
    };
    if let Some(c) = &current {
        let mut same = next.clone();
        same.updated_at_ms = c.updated_at_ms;
        if &same == c {
            return Ok(unchanged(&c.id, c.revision));
        }
    }
    let saved = tx.router().put_route_model(&next, expected)?;
    // A configuration change ends the model's cooldown (its cause may be gone).
    if current.as_ref().is_some_and(|c| {
        c.endpoint_profile != saved.endpoint_profile
            || c.upstream_model_id != saved.upstream_model_id
            || c.connection_id != saved.connection_id
    }) {
        tx.router().clear_cooldown(&saved.id)?;
    }
    event(
        tx,
        actor,
        (MODEL_AGGREGATE, &saved.id, saved.revision),
        "RouteModelConfigured",
        json!({
            "modelId": saved.id.as_str(),
            "endpointProfile": saved.endpoint_profile,
            "upstreamModelId": saved.upstream_model_id,
            "enabled": saved.enabled,
            "efforts": saved.efforts,
            "imageInput": saved.image_input,
            "tools": saved.tools,
            "contextTokens": saved.context_tokens,
            "connectionId": saved.connection_id.as_ref().map(Id::as_str),
        }),
    )?;
    Ok(applied(&saved.id, saved.revision))
}

/// `Tier`: the members of one tier, saved as its routing setting (the workspace settings revision
/// is the expected revision).
fn configure_tier(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    tier: &TierName,
    expected: i64,
    members: &[Id],
) -> Result<DomainDecision, Failure> {
    let name = wire(tier);
    let (_, key) = TIERS
        .iter()
        .find(|(t, _)| *t == name)
        .ok_or_else(|| invalid("tier"))?;
    let mut seen = BTreeSet::new();
    for id in members {
        if !seen.insert(id.as_str()) {
            return Err(invalid("member_ids"));
        }
        if tx.router().route_model(id)?.is_none() {
            // Decision models are never tier members; unknown models cannot be pooled.
            return Err(refuse(DomainErrorCode::NotFound, MODEL_AGGREGATE, Some(id)));
        }
    }
    let ids: Vec<&str> = members.iter().map(Id::as_str).collect();
    let tier_id = Id::new(name.clone()).map_err(|_| invalid("tier"))?;
    match crate::settings::service::save_workspace_value(tx, ctx, key, json!(ids), expected)? {
        Ok(Some(revision)) => Ok(applied(&tier_id, revision)),
        Ok(None) => Ok(unchanged(&tier_id, expected)),
        Err((code, kind)) => Err(refuse(code, kind, Some(&tier_id))),
    }
}

/// `DecisionModel`: a SystemOne decision model, its own model ID (never a tier member).
fn configure_decision_model(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    model: &DecisionModel,
    expected: Option<i64>,
) -> Result<DomainDecision, Failure> {
    if tx.router().route_model(&model.id)?.is_some() {
        return Err(refuse(
            DomainErrorCode::Conflict,
            MODEL_AGGREGATE,
            Some(&model.id),
        ));
    }
    match tx.router().connection(&model.connection_id)? {
        None => {
            return Err(refuse(
                DomainErrorCode::NotFound,
                CONNECTION,
                Some(&model.connection_id),
            ));
        }
        Some(c) if c.profile != "SystemOne" => {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "connection_profile",
                Some(&model.connection_id),
            ));
        }
        Some(_) => {}
    }
    let types: Vec<String> = model.question_types.0.iter().map(wire).collect();
    let unique: BTreeSet<&String> = types.iter().collect();
    if types.is_empty() || unique.len() != types.len() {
        return Err(invalid("question_types"));
    }
    let current = tx.router().decision_model(&model.id)?;
    match (&current, expected) {
        (Some(_), None) => {
            return Err(refuse(
                DomainErrorCode::Conflict,
                DECISION_AGGREGATE,
                Some(&model.id),
            ));
        }
        (None, Some(_)) => {
            return Err(refuse(
                DomainErrorCode::NotFound,
                DECISION_AGGREGATE,
                Some(&model.id),
            ));
        }
        (Some(c), Some(e)) if c.revision != e => {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                DECISION_AGGREGATE,
                model.id.clone(),
                rev(e),
                rev(c.revision),
            )));
        }
        _ => {}
    }
    let next = StoredDecisionModel {
        id: model.id.clone(),
        display_name: model.display_name.0.clone(),
        connection_id: model.connection_id.clone(),
        upstream_model_id: model.upstream_model_id.0.clone(),
        question_types: types,
        accepts_images: model.accepts_images,
        accepts_videos: model.accepts_videos,
        enabled: model.enabled,
        extension_schemas: serde_json::to_string(&model.extension_question_schemas)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        result_schema: serde_json::to_string(&model.result_schema)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        revision: current.as_ref().map_or(1, |c| c.revision),
        updated_at_ms: tx.now_ms(),
    };
    if let Some(c) = &current {
        let mut same = next.clone();
        same.updated_at_ms = c.updated_at_ms;
        if &same == c {
            return Ok(unchanged(&c.id, c.revision));
        }
    }
    let saved = tx.router().put_decision_model(&next, expected)?;
    event(
        tx,
        actor,
        (DECISION_AGGREGATE, &saved.id, saved.revision),
        "DecisionModelConfigured",
        json!({
            "modelId": saved.id.as_str(),
            "connectionId": saved.connection_id.as_str(),
            "upstreamModelId": saved.upstream_model_id,
            "questionTypes": saved.question_types,
            "acceptsImages": saved.accepts_images,
            "acceptsVideos": saved.accepts_videos,
            "enabled": saved.enabled,
        }),
    )?;
    Ok(applied(&saved.id, saved.revision))
}

// ---------------------------------------------------------------------------------------------
// Ports

/// The prefix of settings secret references (`dwsecret-`).
const SETTING_SECRET_PREFIX: &str = "dwsecret-";

/// The production [`SecretResolver`]: settings references resolve from the same OS secret
/// store, in this store's namespace (one secret-store abstraction).
impl SecretResolver for ConnectionService {
    fn resolve(&self, reference: &SecretRef) -> Result<SecretValue, SecretUnavailable> {
        let handle = reference_handle(reference).ok_or(SecretUnavailable::NotFound)?;
        match self.secrets.get(&self.key(&handle)) {
            Ok(Some(s)) => Ok(SecretValue::new(s.expose())),
            Ok(None) => Err(SecretUnavailable::NotFound),
            Err(SecretError::Unavailable(_)) | Err(SecretError::Denied) => {
                Err(SecretUnavailable::Locked)
            }
            Err(_) => Err(SecretUnavailable::Locked),
        }
    }
}

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
                    id: c.id.clone(),
                    profile: c.profile.clone(),
                    state,
                    discovered: c
                        .discovered
                        .as_ref()
                        .map(|d| d.iter().map(|m| m.id.clone()).collect()),
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
        let prep = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
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
        let outcome = match command.input() {
            OperationInput::Router(ops::Input::Configure(i)) => {
                let slot = command.prepared::<Arc<Mutex<Option<Prep>>>>();
                let guard = slot.map(|s| s.lock().unwrap_or_else(|p| p.into_inner()));
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
