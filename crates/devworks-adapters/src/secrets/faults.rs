//! What each OS facility's errors mean for DevWorks, as pure functions compiled and tested on
//! every platform. A facility that cannot be used right now (locked and not unlocked, missing,
//! not running, the user declined to unlock it) is [`SecretError::Unavailable`]: affected
//! connections wait and Settings offers the session-only fallback. A refused access is
//! [`SecretError::Denied`]. Anything else is a defect of the call ([`SecretError::Failed`]) and
//! never offers the fallback.
use devworks_core::ports::secrets::SecretError;

/// A Secret Service (D-Bus) failure: a named error reply, a broken bus connection, or anything
/// else (a malformed reply, a serialization error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub(crate) enum BusFault<'a> {
    Named(&'a str),
    Connection,
    Other,
}

/// The meaning of a Secret Service failure.
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub(crate) fn secret_service(fault: BusFault<'_>) -> SecretError {
    let unavailable = |why: &str| SecretError::Unavailable(why.into());
    match fault {
        BusFault::Named("org.freedesktop.Secret.Error.IsLocked") => {
            unavailable("the keyring is locked")
        }
        BusFault::Named("org.freedesktop.Secret.Error.NoSession") => {
            unavailable("the Secret Service session ended")
        }
        // The collection (or keyring) went away under the call.
        BusFault::Named(
            "org.freedesktop.Secret.Error.NoSuchObject"
            | "org.freedesktop.DBus.Error.UnknownObject",
        ) => unavailable("the keyring is not available"),
        BusFault::Named(
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.Spawn.ServiceNotFound"
            | "org.freedesktop.DBus.Error.Spawn.ChildExited"
            | "org.freedesktop.DBus.Error.Spawn.ExecFailed",
        ) => unavailable("no Secret Service is running"),
        BusFault::Named(
            "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Timeout"
            | "org.freedesktop.DBus.Error.TimedOut"
            | "org.freedesktop.DBus.Error.Disconnected",
        ) => unavailable("the Secret Service did not answer"),
        BusFault::Named(
            "org.freedesktop.DBus.Error.AccessDenied" | "org.freedesktop.DBus.Error.AuthFailed",
        ) => SecretError::Denied,
        BusFault::Named(name) => SecretError::Failed(name.into()),
        BusFault::Connection => unavailable("the session bus connection failed"),
        BusFault::Other => SecretError::Failed("the Secret Service call failed".into()),
    }
}

/// Whether a Secret Service error says the collection or item is locked (unlock and retry).
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub(crate) fn locked(name: &str) -> bool {
    name == "org.freedesktop.Secret.Error.IsLocked"
}

/// Whether a Secret Service error on an item says it no longer exists (deleted meanwhile).
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub(crate) fn gone(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.Secret.Error.NoSuchObject" | "org.freedesktop.DBus.Error.UnknownObject"
    )
}

/// The meaning of a Security framework status (never `errSecSuccess`, `errSecItemNotFound` or
/// `errSecDuplicateItem`, which each call handles itself).
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
pub(crate) fn keychain(status: i32) -> SecretError {
    match status {
        // A locked keychain while no unlock prompt can be shown.
        -25308 => SecretError::Unavailable("the keychain is locked".into()),
        // The user dismissed the unlock prompt: the keychain cannot be used this time.
        -128 => SecretError::Unavailable("unlocking the keychain was cancelled".into()),
        // errSecNoSuchKeychain, errSecNotAvailable, errSecNoDefaultKeychain.
        -25294 | -25291 | -25307 => SecretError::Unavailable("no keychain is available".into()),
        // errSecReadOnly: nothing can be stored in it.
        -25292 => SecretError::Unavailable("the keychain is read-only".into()),
        // errSecMissingEntitlement: this build cannot use the keychain at all.
        -34018 => SecretError::Unavailable("this app cannot use the keychain".into()),
        // errSecAuthFailed: the user could not authenticate, or the item's access list refused.
        -25293 => SecretError::Denied,
        s => SecretError::Failed(format!("OSStatus {s}")),
    }
}

/// The meaning of a Credential Manager (Win32) error (never `ERROR_NOT_FOUND`, which each call
/// handles itself).
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub(crate) fn credential_manager(code: u32) -> SecretError {
    match code {
        // ERROR_NO_SUCH_LOGON_SESSION: no profile holds a credential store (a service session).
        1312 => SecretError::Unavailable("no user logon session holds a credential store".into()),
        // RPC_S_SERVER_UNAVAILABLE / RPC_S_CALL_FAILED: the Credential Manager service is down.
        1722 | 1726 => SecretError::Unavailable("the Credential Manager is not running".into()),
        // ERROR_ACCESS_DENIED.
        5 => SecretError::Denied,
        c => SecretError::Failed(format!("Win32 error {c}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable(e: &SecretError) -> bool {
        matches!(e, SecretError::Unavailable(_))
    }

    #[test]
    fn a_locked_or_absent_secret_service_is_unavailable() {
        for name in [
            "org.freedesktop.Secret.Error.IsLocked",
            "org.freedesktop.Secret.Error.NoSession",
            "org.freedesktop.Secret.Error.NoSuchObject",
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.Disconnected",
        ] {
            assert!(
                unavailable(&secret_service(BusFault::Named(name))),
                "{name}"
            );
        }
        assert!(unavailable(&secret_service(BusFault::Connection)));
        // Unlock-and-retry and vanished items are told apart from other failures.
        assert!(locked("org.freedesktop.Secret.Error.IsLocked"));
        assert!(!locked("org.freedesktop.Secret.Error.NoSession"));
        assert!(gone("org.freedesktop.Secret.Error.NoSuchObject"));
        assert!(gone("org.freedesktop.DBus.Error.UnknownObject"));
        assert!(!gone("org.freedesktop.Secret.Error.IsLocked"));
        assert_eq!(
            secret_service(BusFault::Named("org.freedesktop.DBus.Error.AccessDenied")),
            SecretError::Denied
        );
        // A defect of the call never offers the fallback.
        for fault in [
            BusFault::Named("org.freedesktop.DBus.Error.InvalidArgs"),
            BusFault::Named("org.freedesktop.DBus.Error.UnknownMethod"),
            BusFault::Other,
        ] {
            assert!(
                matches!(secret_service(fault), SecretError::Failed(_)),
                "{fault:?}"
            );
        }
    }

    #[test]
    fn a_locked_cancelled_or_missing_keychain_is_unavailable() {
        for s in [-25308, -128, -25294, -25291, -25307, -25292, -34018] {
            assert!(unavailable(&keychain(s)), "{s}");
        }
        assert_eq!(keychain(-25293), SecretError::Denied);
        assert!(
            matches!(keychain(-50), SecretError::Failed(_)),
            "errSecParam is a defect"
        );
    }

    #[test]
    fn a_credential_manager_without_a_logon_session_or_service_is_unavailable() {
        for c in [1312, 1722, 1726] {
            assert!(unavailable(&credential_manager(c)), "{c}");
        }
        assert_eq!(credential_manager(5), SecretError::Denied);
        // ERROR_INVALID_PARAMETER, ERROR_INVALID_FLAGS, ERROR_BAD_USERNAME: defects.
        for c in [87, 1004, 2202] {
            assert!(
                matches!(credential_manager(c), SecretError::Failed(_)),
                "{c}"
            );
        }
    }
}
