//! Native [`SecretStore`] adapters (SEC-01, AD 39, AD 73, DW-18): the macOS Keychain, the
//! Windows Credential Manager and the Linux Secret Service over the user's session bus. Each
//! stores one generic item per handle under the service/target `DevWorks` and the account
//! `<namespace>/<handle>`, never logs or returns a value in an error, and reports a locked,
//! missing or refusing facility as [`SecretError::Unavailable`] so affected connections wait
//! while the rest of the app keeps working. There is no plaintext disk fallback: the only
//! alternative is the explicit, session-only [`devworks_core::ports::secrets::SessionSecrets`].
//!
//! The adapters are exercised by opt-in local tests (`--ignored`), never in CI: they touch the
//! real user facility.
use devworks_core::ports::secrets::SecretStore;

#[cfg(target_os = "macos")]
mod keychain;
#[cfg(all(unix, not(target_os = "macos")))]
mod secret_service;
#[cfg(windows)]
mod windows;

/// The service name (macOS), target prefix (Windows) and application attribute (Linux).
pub const SERVICE: &str = "DevWorks";

/// This platform's OS secret facility.
pub fn native() -> Box<dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        Box::new(keychain::Keychain)
    }
    #[cfg(windows)]
    {
        Box::new(windows::CredentialManager)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Box::new(secret_service::SecretService)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Box::new(Unsupported)
    }
}

/// A platform without a supported facility: every call is unavailable.
#[cfg(not(any(unix, windows)))]
struct Unsupported;
#[cfg(not(any(unix, windows)))]
impl SecretStore for Unsupported {
    fn name(&self) -> &'static str {
        "unsupported"
    }
    fn put(
        &self,
        _: &devworks_core::ports::secrets::SecretKey,
        _: &devworks_core::ports::secrets::Secret,
    ) -> Result<(), devworks_core::ports::secrets::SecretError> {
        Err(unsupported())
    }
    fn get(
        &self,
        _: &devworks_core::ports::secrets::SecretKey,
    ) -> Result<
        Option<devworks_core::ports::secrets::Secret>,
        devworks_core::ports::secrets::SecretError,
    > {
        Err(unsupported())
    }
    fn delete(
        &self,
        _: &devworks_core::ports::secrets::SecretKey,
    ) -> Result<bool, devworks_core::ports::secrets::SecretError> {
        Err(unsupported())
    }
    fn list(
        &self,
        _: &str,
    ) -> Result<Vec<devworks_core::protocol::Id>, devworks_core::ports::secrets::SecretError> {
        Err(unsupported())
    }
}
#[cfg(not(any(unix, windows)))]
fn unsupported() -> devworks_core::ports::secrets::SecretError {
    devworks_core::ports::secrets::SecretError::Unavailable(
        "no supported secret store on this platform".into(),
    )
}

/// Parses `<namespace>/<handle>` back into the handle when it belongs to `namespace`.
pub(crate) fn handle_of(account: &str, namespace: &str) -> Option<devworks_core::protocol::Id> {
    let rest = account.strip_prefix(namespace)?.strip_prefix('/')?;
    devworks_core::protocol::Id::new(rest).ok()
}

/// The opt-in round trip every adapter must pass against the real facility.
#[cfg(test)]
pub(crate) fn round_trip(store: &dyn SecretStore) {
    use devworks_core::ports::secrets::{Secret, SecretKey};
    use devworks_core::protocol::Id;
    let ns = format!("devworks-test-{}", std::process::id());
    let a = SecretKey::new(ns.clone(), Id::new("handle-a").unwrap());
    let b = SecretKey::new(ns.clone(), Id::new("handle-b").unwrap());
    let _ = store.delete(&a);
    let _ = store.delete(&b);
    assert_eq!(store.get(&a).unwrap(), None);
    store.put(&a, &Secret::new("first")).unwrap();
    store.put(&a, &Secret::new("second")).unwrap();
    assert_eq!(store.get(&a).unwrap(), Some(Secret::new("second")));
    // Larger than one Windows credential blob (2560 bytes).
    let long = "t".repeat(9000);
    store.put(&b, &Secret::new(long.clone())).unwrap();
    assert_eq!(store.get(&b).unwrap(), Some(Secret::new(long)));
    let mut listed: Vec<String> = store
        .list(&ns)
        .unwrap()
        .into_iter()
        .map(|h| h.as_str().to_owned())
        .collect();
    listed.sort();
    assert_eq!(listed, ["handle-a", "handle-b"]);
    assert!(store.delete(&a).unwrap());
    assert!(!store.delete(&a).unwrap());
    assert!(store.delete(&b).unwrap());
    assert!(store.list(&ns).unwrap().is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_parse_back_only_within_their_namespace() {
        assert_eq!(
            handle_of("store-a/h1", "store-a").map(|h| h.as_str().to_owned()),
            Some("h1".into())
        );
        assert_eq!(handle_of("store-ab/h1", "store-a"), None);
        assert_eq!(handle_of("store-a/h1/part1", "store-a"), None);
    }

    /// Exercises the real OS facility. Opt-in: `cargo test -p devworks-adapters -- --ignored
    /// native_secret_store`. CI never runs it, and a skip is reported as ignored, not passed.
    #[test]
    #[ignore = "touches the real OS secret store; run locally with --ignored"]
    fn native_secret_store_round_trip() {
        round_trip(&*native());
    }
}
