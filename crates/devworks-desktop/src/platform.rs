//! Production host ports the composition root supplies: the wall clock, opaque identifiers, the
//! host authority over the one store, and the per-user data root.
use devworks_core::authority::HostAuthority;
use devworks_core::context::InvocationContext;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::ApplicationStore;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock, IdKind, IdSource,
};
use devworks_core::protocol::{Counter, Id};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The operating system wall clock in Unix milliseconds. The engine detects sleep/wake by
/// comparing it with Tokio's monotonic clock, so it is read, never adjusted, here.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> Counter {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Counter::new(ms).unwrap_or_else(|_| Counter::new(0).expect("zero is a counter"))
    }
}

/// Opaque identifiers: `<kind>-<24 hex>` from SHA-256 over a per-process seed and a counter.
/// IDs carry no order, authority or path meaning (the seed mixes the launch instant, the process
/// and an address, so two launches never share a sequence).
pub struct HostIds {
    seed: String,
    next: AtomicU64,
}
impl HostIds {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        static INSTANCE: AtomicU64 = AtomicU64::new(0);
        let local = 0u8;
        let seed = format!(
            "{nanos}:{}:{:p}:{}",
            std::process::id(),
            &local,
            INSTANCE.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            seed: sha256_hex(seed.as_bytes()),
            next: AtomicU64::new(0),
        }
    }
    /// A fresh opaque token (request keys, query IDs) from the same sequence.
    pub fn token(&self, prefix: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let digest = sha256_hex(format!("{}:{n}", self.seed).as_bytes());
        format!("{prefix}-{}", &digest[..24])
    }
}
impl Default for HostIds {
    fn default() -> Self {
        Self::new()
    }
}
impl IdSource for HostIds {
    fn next_id(&self, kind: IdKind) -> Id {
        // `Debug` keeps this total over kinds other items add later.
        let kind = format!("{kind:?}").to_lowercase();
        Id::new(self.token(&kind)).expect("hex identifiers are valid")
    }
}

/// The production [`AuthorityPort`]: [`HostAuthority`] over the one store and clock, owned so it
/// can live in the runtime composition (the core type borrows its ports).
pub struct StoreAuthority {
    pub store: Arc<dyn ApplicationStore>,
    pub clock: Arc<dyn Clock>,
}
impl AuthorityPort for StoreAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            // No names beyond main and master are configured: the authority adds the pushed
            // repository's recorded default branch itself.
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

/// Environment variable that overrides the data root (tests, portable installs).
pub const DATA_DIR_ENV: &str = "DEVWORKS_DATA_DIR";

/// The per-user data root: `DEVWORKS_DATA_DIR` when set, otherwise the platform convention
/// (`~/Library/Application Support/DevWorks`, `%APPDATA%\DevWorks`, or
/// `$XDG_DATA_HOME/devworks` falling back to `~/.local/share/devworks`). `None` when no home
/// directory can be determined; the shell then reports the store as unavailable instead of
/// guessing a location.
pub fn default_data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support/DevWorks"))
    } else if cfg!(windows) {
        var("APPDATA").map(|a| a.join("DevWorks"))
    } else {
        var("XDG_DATA_HOME")
            .map(|d| d.join("devworks"))
            .or_else(|| var("HOME").map(|h| h.join(".local/share/devworks")))
    }
}

/// The local user's display name for attribution (`USER`/`USERNAME`), or "You".
pub fn user_display_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "You".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_valid_distinct_and_kind_prefixed() {
        let ids = HostIds::new();
        let a = ids.next_id(IdKind::Session);
        let b = ids.next_id(IdKind::Session);
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("session-"), "{a:?}");
        let other = HostIds::new();
        assert_ne!(
            other.next_id(IdKind::Session),
            a,
            "launches never share a sequence"
        );
    }

    #[test]
    fn the_clock_reads_unix_milliseconds() {
        assert!(SystemClock.now_ms().get() > 1_600_000_000_000);
    }
}
