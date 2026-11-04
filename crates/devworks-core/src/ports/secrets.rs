//! Resolution of host-minted secret references (`{"secretRef": "dwsecret-..."}` settings values)
//! to the credential they name, for the one adapter that sends it.
//!
//! Records and settings only ever hold the opaque [`SecretRef`]; the value lives in the native OS
//! secret store, whose adapter and secure entry flow belong to the provider-credential item
//! (DW-18). Until that adapter is composed, the production host uses [`NoSecretStore`], so a
//! feature that needs a credential reports `CredentialsRequired` instead of reading one from
//! anywhere else.
pub use crate::authority::redact::SecretRef;
use std::fmt;

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

/// The native secret store, read side.
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &SecretRef) -> Result<SecretValue, SecretUnavailable>;
}

/// No native secret store is composed (the production state until DW-18).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSecretStore;
impl SecretResolver for NoSecretStore {
    fn resolve(&self, _: &SecretRef) -> Result<SecretValue, SecretUnavailable> {
        Err(SecretUnavailable::Unsupported)
    }
}
