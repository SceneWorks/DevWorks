//! The OS secret facility (SEC-01, AD 39, AD 73): API keys, OAuth tokens and settings
//! credentials live only here, never in SQLite, logs, diagnostics, receipts or results.
//! Application records hold an opaque handle instead: a provider connection its `credential_ref`
//! and credential revision, a setting such as `web.searchCredential` a host-minted
//! `{"secretRef": "dwsecret-..."}` reference.
//!
//! One abstraction with two sides. [`SecretStore`] is the facility itself (write, read, delete,
//! list by namespace). [`SecretResolver`] is the read side for settings references, for the one
//! adapter that sends the credential (the web search provider). The production resolver is the
//! provider connection service over the host's [`SecretStore`] (it stores a settings secret under
//! its reference as the handle), so the desktop host's web tools resolve `web.searchCredential`
//! from the same OS facility as the provider credentials. [`NoSecretStore`] answers only where no
//! native store is composed (a composition without the connection service); a feature that needs
//! a credential then reports `CredentialsRequired` instead of reading one from anywhere else.
//!
//! Handles are immutable: a replaced credential is written under a new handle, published by a
//! compare-and-set of the connection's credential revision, and the old handle is then deleted.
//! Secret storage and SQLite are not one transaction, so a handle written but never published (a
//! refused or interrupted command) is an orphan that [`SecretStore::list`] lets the owner
//! reconcile without reading or printing any value.
//!
//! Production adapters live in `devworks-adapters::secrets` (macOS Keychain, Windows Credential
//! Manager, Linux Secret Service; on any other platform every call is unavailable).
//! [`SessionSecrets`] keeps secrets in memory for the current app session only: it is the test
//! substitute and the explicit, user-chosen fallback when the OS facility is unavailable, never
//! a silent plaintext disk fallback.
use crate::protocol::Id;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

pub use crate::authority::redact::SecretRef;

// ---------------------------------------------------------------------------------------------
// Read side for settings credentials (`{"secretRef": "dwsecret-..."}` values), which the web
// tools resolve their search credential through.

/// A resolved credential. Its `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);
impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// The value, for the one request that must carry it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(..)")
    }
}

/// Why a reference did not resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretUnavailable {
    /// The store holds nothing under the reference.
    NotFound,
    /// The store is locked or refused access.
    Locked,
    /// No native secret store is available in this build.
    Unsupported,
}

/// The native secret store, read side, for settings secret references.
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &SecretRef) -> Result<SecretValue, SecretUnavailable>;
}

/// No native secret store is composed: every reference is `Unsupported`. Production hosts
/// compose the provider connection service instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSecretStore;
impl SecretResolver for NoSecretStore {
    fn resolve(&self, _: &SecretRef) -> Result<SecretValue, SecretUnavailable> {
        Err(SecretUnavailable::Unsupported)
    }
}

/// The handle a settings secret reference is stored under (`dwsecret-<hex>` is a valid ID).
pub fn reference_handle(reference: &SecretRef) -> Option<Id> {
    Id::new(reference.as_str()).ok()
}

/// A secret value. Its `Debug` and `Display` never show the value, and the buffer is cleared
/// when it is dropped (best effort: copies made by the OS facility or the network stack are
/// outside its control).
pub struct Secret(String);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// The value, for the one adapter boundary that sends or stores it.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl Clone for Secret {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Secret {}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.iter_mut().for_each(|b| *b = 0);
        // Keeps the clear of a dying buffer from being optimized away.
        std::hint::black_box(&bytes);
    }
}

/// Where one secret is kept: the store's namespace (its store ID, so two data roots never share
/// or reconcile each other's secrets) and the immutable handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretKey {
    pub namespace: String,
    pub handle: Id,
}
impl SecretKey {
    pub fn new(namespace: impl Into<String>, handle: Id) -> Self {
        Self {
            namespace: namespace.into(),
            handle,
        }
    }
    /// The account/target name the adapters store under: `<namespace>/<handle>`.
    pub fn account(&self) -> String {
        format!("{}/{}", self.namespace, self.handle.as_str())
    }
}

/// Why the secret facility could not do something. Never carries a secret value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretError {
    /// The facility is missing, locked or refused interaction (CredentialsUnavailable): affected
    /// connections wait; the UI and unrelated work keep operating.
    Unavailable(String),
    /// The user or the OS denied access to the item.
    Denied,
    /// The value exceeds what the facility can hold.
    TooLarge,
    /// Any other failure, described by its facility status code only.
    Failed(String),
}
impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "the secret store is unavailable ({why})"),
            Self::Denied => f.write_str("access to the secret store was denied"),
            Self::TooLarge => f.write_str("the credential is too large for the secret store"),
            Self::Failed(code) => write!(f, "the secret store failed ({code})"),
        }
    }
}

/// The OS secret facility.
pub trait SecretStore: Send + Sync {
    /// A short human name of the facility ("macOS Keychain", ...).
    fn name(&self) -> &'static str;
    /// Writes `secret` under `key`, replacing a value already stored there.
    fn put(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError>;
    /// The secret under `key`; `None` when nothing is stored.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError>;
    /// Removes `key`; `false` when nothing was stored.
    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError>;
    /// Every handle stored in `namespace` (no values), in no particular order.
    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError>;
}

/// Secrets kept in memory for this app session only (tests, and the explicit session-only
/// fallback). Nothing is ever written to disk.
#[derive(Default)]
pub struct SessionSecrets {
    items: Mutex<BTreeMap<(String, String), Secret>>,
    unavailable: Mutex<Option<String>>,
}
impl SessionSecrets {
    pub fn new() -> Self {
        Self::default()
    }
    /// Makes every call fail as a locked or missing facility would (tests).
    pub fn set_unavailable(&self, why: Option<&str>) {
        *self.unavailable.lock().expect("secrets") = why.map(str::to_owned);
    }
    fn check(&self) -> Result<(), SecretError> {
        match self.unavailable.lock().expect("secrets").as_ref() {
            Some(why) => Err(SecretError::Unavailable(why.clone())),
            None => Ok(()),
        }
    }
    /// The number of stored secrets (tests).
    pub fn len(&self) -> usize {
        self.items.lock().expect("secrets").len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
fn slot(key: &SecretKey) -> (String, String) {
    (key.namespace.clone(), key.handle.as_str().to_owned())
}
impl SecretStore for SessionSecrets {
    fn name(&self) -> &'static str {
        "session-only memory"
    }
    fn put(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        self.check()?;
        self.items
            .lock()
            .expect("secrets")
            .insert(slot(key), secret.clone());
        Ok(())
    }
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.check()?;
        Ok(self.items.lock().expect("secrets").get(&slot(key)).cloned())
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        self.check()?;
        Ok(self
            .items
            .lock()
            .expect("secrets")
            .remove(&slot(key))
            .is_some())
    }
    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        self.check()?;
        Ok(self
            .items
            .lock()
            .expect("secrets")
            .keys()
            .filter(|(ns, _)| ns == namespace)
            .filter_map(|(_, h)| Id::new(h.clone()).ok())
            .collect())
    }
}

/// The secret store the host uses: the OS facility, or session-only memory once the user chose
/// it explicitly (because the facility is unavailable). Switching never copies or migrates a
/// secret, and it only lasts for this app session.
pub struct SwitchableSecrets {
    native: Box<dyn SecretStore>,
    session: SessionSecrets,
    session_only: std::sync::atomic::AtomicBool,
}
impl SwitchableSecrets {
    pub fn new(native: Box<dyn SecretStore>) -> Self {
        Self {
            native,
            session: SessionSecrets::new(),
            session_only: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// The user's explicit choice to keep credentials in memory for this session only.
    pub fn use_session_only(&self) {
        self.session_only
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_session_only(&self) -> bool {
        self.session_only.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn active(&self) -> &dyn SecretStore {
        if self.is_session_only() {
            &self.session
        } else {
            &*self.native
        }
    }
}
impl SecretStore for SwitchableSecrets {
    fn name(&self) -> &'static str {
        self.active().name()
    }
    fn put(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        self.active().put(key, secret)
    }
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.active().get(key)
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        self.active().delete(key)
    }
    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        self.active().list(namespace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print_their_value() {
        let s = Secret::new("sk-ant-very-secret");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        let e = SecretError::Unavailable("locked".into());
        assert!(!e.to_string().contains("sk-ant"));
    }

    #[test]
    fn session_secrets_are_namespaced_and_can_be_unavailable() {
        let store = SessionSecrets::new();
        let a = SecretKey::new("store-a", Id::new("h1").unwrap());
        let b = SecretKey::new("store-b", Id::new("h2").unwrap());
        store.put(&a, &Secret::new("one")).unwrap();
        store.put(&b, &Secret::new("two")).unwrap();
        assert_eq!(store.list("store-a").unwrap(), vec![Id::new("h1").unwrap()]);
        assert_eq!(store.get(&a).unwrap(), Some(Secret::new("one")));
        store.set_unavailable(Some("locked"));
        assert!(matches!(store.get(&a), Err(SecretError::Unavailable(_))));
        store.set_unavailable(None);
        assert!(store.delete(&a).unwrap());
        assert!(!store.delete(&a).unwrap());
    }
}
