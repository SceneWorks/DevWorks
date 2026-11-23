//! Manual pairing uses the protected OS facility independently of other credentials.
//! A verified credential is short-lived evidence for one application request, not a session.

use super::{Client, Configuration, PublishedTool, current, published_tools};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::ports::http::{Entropy, base64url};
use crate::ports::secrets::{Secret, SecretKey, SecretStore};
use crate::ports::storage::{ApplicationStore, ReadOptions, StorageError, read_snapshot};
use crate::protocol::records::{McpInboundSettings, McpServerBinding};
use crate::protocol::{Id, Revision, StoreId};
use crate::registry::Mode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

const CREDENTIAL_LIFETIME_MS: i64 = 90 * 24 * 60 * 60 * 1_000;
const TOKEN_VERSION: &str = "dw-mcp1";

/// Transport selected by the host after checking the native connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Binding {
    LocalIpc,
    LoopbackHttp { port: u16 },
}

impl Binding {
    pub(crate) fn enabled(self, configuration: &Configuration, epoch: i64) -> bool {
        configuration.settings.enabled
            && configuration.authorized_epoch == Some(epoch)
            && configuration
                .settings
                .bindings
                .0
                .iter()
                .any(|binding| match (self, binding) {
                    (Self::LocalIpc, McpServerBinding::Stdio) => true,
                    (Self::LoopbackHttp { port }, McpServerBinding::LoopbackHttp) => configuration
                        .settings
                        .http_port
                        .as_ref()
                        .is_some_and(|current| current.get() == u32::from(port)),
                    _ => false,
                })
    }
}

#[derive(Debug)]
pub enum CredentialError {
    Closed,
    Denied,
    Expired,
    Unavailable,
    Storage(StorageError),
}

impl From<StorageError> for CredentialError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}

/// Only successful verification in the native credential service produces this proof.
pub struct AuthenticatedClient {
    pub(super) id: Id,
    pub(super) store: StoreId,
    pub(super) epoch: i64,
    pub(super) settings_revision: Revision,
    pub(super) client_revision: Revision,
    pub(super) generation: Revision,
    pub(super) binding: Binding,
    pub(super) expires_ms: i64,
    pub(super) live: Arc<AtomicBool>,
}

impl AuthenticatedClient {
    /// Stable registration identifier for native request budgets.
    pub fn id(&self) -> &Id {
        &self.id
    }
}

pub struct Credentials {
    store: Arc<dyn ApplicationStore>,
    protected: Arc<dyn SecretStore>,
    entropy: Arc<dyn Entropy>,
    provision: Mutex<()>,
    live: Arc<AtomicBool>,
}

/// No secret is retained in a registration or updated from a transport request.
impl Credentials {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        protected: Box<dyn SecretStore>,
        entropy: Arc<dyn Entropy>,
    ) -> Self {
        Self::with_secrets(store, protected.into(), entropy)
    }

    pub fn with_secrets(
        store: Arc<dyn ApplicationStore>,
        protected: Arc<dyn SecretStore>,
        entropy: Arc<dyn Entropy>,
    ) -> Self {
        Self {
            store,
            protected,
            entropy,
            provision: Mutex::new(()),
            live: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn close(&self) {
        self.live.store(false, Ordering::Release);
    }

    /// Effective native settings, including restore's disabled state, without credential data.
    pub fn settings(&self, options: &ReadOptions) -> Result<McpInboundSettings, CredentialError> {
        self.require_open()?;
        let result = read_snapshot(&*self.store, options, |snapshot| {
            let (_, config) = current(snapshot.configuration())?;
            Ok(config.status(snapshot.stamp().restore_epoch).settings)
        })?;
        self.require_open()?;
        Ok(result)
    }

    fn require_open(&self) -> Result<(), CredentialError> {
        if self.live.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(CredentialError::Closed)
        }
    }

    /// The trusted desktop reveals the credential only after an explicit private action.
    /// Repeating the action in one generation returns the same credential. An expired
    /// credential requires durable generation rotation before it can be replaced.
    pub fn provision(
        &self,
        ctx: &InvocationContext,
        id: &Id,
        revision: &Revision,
    ) -> Result<Secret, CredentialError> {
        self.require_open()?;
        let _guard = self
            .provision
            .lock()
            .map_err(|_| CredentialError::Unavailable)?;
        let (target, now) = self.provision_target(ctx, id, revision)?;
        let key = target.key();
        let secret = if let Some(secret) = self
            .protected
            .get(&key)
            .map_err(|_| CredentialError::Unavailable)?
        {
            usable_secret(&secret, now)?.1
        } else {
            let mut nonce = [0_u8; 32];
            self.entropy.fill(&mut nonce);
            let token = Secret::new(format!(
                "{TOKEN_VERSION}.{}.{}",
                id.as_str(),
                base64url(&nonce)
            ));
            nonce.fill(0);
            std::hint::black_box(&nonce);
            let expires = now
                .checked_add(CREDENTIAL_LIFETIME_MS)
                .ok_or(CredentialError::Unavailable)?;
            self.protected
                .put(&key, &Secret::new(format!("{expires}:{}", token.expose())))
                .map_err(|_| CredentialError::Unavailable)?;
            token
        };
        if self.provision_target(ctx, id, revision)?.0 != target {
            return Err(CredentialError::Denied);
        }
        self.require_open()?;
        crate::authority::redact::register_resolved_secret(secret.expose());
        Ok(secret)
    }

    fn provision_target(
        &self,
        ctx: &InvocationContext,
        id: &Id,
        revision: &Revision,
    ) -> Result<(CredentialTarget, i64), CredentialError> {
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let stamp = snapshot.stamp();
            if ctx.principal_kind() != PrincipalKind::Human
                || ctx.store_id() != &stamp.store_id
                || ctx.restore_epoch().get() != stamp.restore_epoch
                || snapshot.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
            {
                return Ok(None);
            }
            let (_, config) = current(snapshot.configuration())?;
            Ok(config
                .client(id)
                .filter(|client| {
                    client.access.enabled
                        && &client.access.revision == revision
                        && client.authorized_epoch == Some(stamp.restore_epoch)
                })
                .map(|client| {
                    (
                        CredentialTarget::new(&stamp.store_id, stamp.restore_epoch, client),
                        stamp.now_ms,
                    )
                }))
        })?;
        target.ok_or(CredentialError::Denied)
    }

    /// The native boundary checks the socket/peer and request metadata before calling this.
    /// A transport connection or caller-supplied label never stands in for a credential.
    pub fn authenticate(
        &self,
        binding: Binding,
        credential: &Secret,
    ) -> Result<AuthenticatedClient, CredentialError> {
        self.require_open()?;
        let id = parse_token(credential).ok_or(CredentialError::Denied)?;
        let (target, settings_revision, client_revision, now) =
            self.admission_target(binding, &id)?;
        let stored = self
            .protected
            .get(&target.key())
            .map_err(|_| CredentialError::Unavailable)?
            .ok_or(CredentialError::Denied)?;
        let (expires_ms, expected) = usable_secret(&stored, now)?;
        if !bool::from(
            expected
                .expose()
                .as_bytes()
                .ct_eq(credential.expose().as_bytes()),
        ) {
            return Err(CredentialError::Denied);
        }
        let (current, settings, client, now) = self.admission_target(binding, &id)?;
        if current != target || settings != settings_revision || client != client_revision {
            return Err(CredentialError::Denied);
        }
        if now >= expires_ms {
            return Err(CredentialError::Expired);
        }
        self.require_open()?;
        crate::authority::redact::register_resolved_secret(credential.expose());
        Ok(AuthenticatedClient {
            id,
            store: target.store,
            epoch: target.epoch,
            settings_revision,
            client_revision,
            generation: target.generation,
            binding,
            expires_ms,
            live: self.live.clone(),
        })
    }

    /// Discovery uses only current registrations. Capabilities are filtered again by the
    /// desktop composition root against the owners it has installed.
    pub fn granted_tools(
        &self,
        client: &AuthenticatedClient,
        options: &ReadOptions,
    ) -> Result<Vec<&'static PublishedTool>, CredentialError> {
        self.require_open()?;
        if !Arc::ptr_eq(&self.live, &client.live) {
            return Err(CredentialError::Denied);
        }
        let tools = read_snapshot(&*self.store, options, |snapshot| {
            let stamp = snapshot.stamp();
            let (_, config) = current(snapshot.configuration())?;
            let access = config.client(&client.id).filter(|stored| {
                stamp.store_id == client.store
                    && stamp.restore_epoch == client.epoch
                    && stamp.now_ms < client.expires_ms
                    && client.binding.enabled(&config, stamp.restore_epoch)
                    && config.settings.revision == client.settings_revision
                    && stored.authorized_epoch == Some(stamp.restore_epoch)
                    && stored.access.enabled
                    && stored.access.revision == client.client_revision
                    && stored.access.credential_generation == client.generation
            });
            Ok(access.map(|stored| {
                published_tools()
                    .iter()
                    .filter(|tool| {
                        let allowed = match tool.binding.mode {
                            Mode::Query => &stored.access.read_operations,
                            Mode::Command => &stored.access.command_operations,
                        };
                        allowed.0.iter().any(|name| name.0 == tool.operation.name)
                    })
                    .collect()
            }))
        })?;
        self.require_open()?;
        tools.ok_or(CredentialError::Denied)
    }

    fn admission_target(
        &self,
        binding: Binding,
        id: &Id,
    ) -> Result<(CredentialTarget, Revision, Revision, i64), CredentialError> {
        let target = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let stamp = snapshot.stamp();
            let (_, config) = current(snapshot.configuration())?;
            if !binding.enabled(&config, stamp.restore_epoch) {
                return Ok(None);
            }
            Ok(config
                .client(id)
                .filter(|client| {
                    client.access.enabled && client.authorized_epoch == Some(stamp.restore_epoch)
                })
                .map(|client| {
                    (
                        CredentialTarget::new(&stamp.store_id, stamp.restore_epoch, client),
                        config.settings.revision.clone(),
                        client.access.revision.clone(),
                        stamp.now_ms,
                    )
                }))
        })?;
        target.ok_or(CredentialError::Denied)
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(PartialEq, Eq)]
struct CredentialTarget {
    store: StoreId,
    epoch: i64,
    client: Id,
    generation: Revision,
}

impl CredentialTarget {
    fn new(store: &StoreId, epoch: i64, client: &Client) -> Self {
        Self {
            store: store.clone(),
            epoch,
            client: client.access.client_id.clone(),
            generation: client.access.credential_generation.clone(),
        }
    }

    fn key(&self) -> SecretKey {
        let identity = format!("{}:{}", self.client.as_str(), self.generation.as_str());
        SecretKey::new(
            format!("{}-incoming-mcp-{}", self.store.0.as_str(), self.epoch),
            Id::new(sha256_hex(identity.as_bytes())).expect("hex digest is an ID"),
        )
    }
}

fn parse_token(credential: &Secret) -> Option<Id> {
    let mut parts = credential.expose().split('.');
    let (Some(TOKEN_VERSION), Some(client), Some(nonce), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if nonce.len() != 43
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return None;
    }
    Id::new(client).ok()
}

fn usable_secret(secret: &Secret, now_ms: i64) -> Result<(i64, Secret), CredentialError> {
    let (expires, token) = secret
        .expose()
        .split_once(':')
        .ok_or(CredentialError::Unavailable)?;
    let expires: i64 = expires.parse().map_err(|_| CredentialError::Unavailable)?;
    if now_ms >= expires {
        return Err(CredentialError::Expired);
    }
    Ok((expires, Secret::new(token)))
}
