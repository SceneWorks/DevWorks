//! The provider connections the inference router routes through.
//!
//! Connection records, sign-in and secret handles belong to the provider-connection service
//! (`crate::services::router::connections`, DW-18); the transports are DW-19. The router only
//! needs to know, at selection time, which connections are usable, which endpoint profile each
//! serves and (once discovered) which upstream models each lists, so it reads them through this
//! port and never through a provider. The port is synchronous and reads the durable connection
//! rows of the caller's own snapshot or unit of work (plus in-process sign-in/refresh state), so
//! a route decided in a command sees exactly the connections committed with it: it never touches
//! the network and never serves a cached copy that a later settings change made stale.
use crate::ports::storage::{RouterRead, StorageError};
use crate::protocol::Id;
use std::collections::BTreeSet;

/// A connection's usability as the router sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Authenticated and usable.
    Connected,
    /// Never signed in, or signed out.
    Disconnected,
    /// A sign-in is in progress.
    Authorizing,
    /// Refreshing its credential; requests wait for the refresh.
    Refreshing,
    /// The provider rejected the credential; the user must sign in again.
    ReauthenticationRequired,
    /// Temporarily unreachable (an availability event, not a revocation).
    Unavailable,
    /// Turned off by the user, or a subscription profile whose risk notice is not accepted.
    Disabled,
}
impl ConnectionState {
    /// The wire `ConnectionLifecycle` name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Disconnected => "Disconnected",
            Self::Authorizing => "Authorizing",
            Self::Refreshing => "Refreshing",
            Self::ReauthenticationRequired => "ReauthenticationRequired",
            Self::Unavailable => "Unavailable",
            Self::Disabled => "Disabled",
        }
    }
}

/// One connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub id: Id,
    /// The `EndpointKind` wire name it serves.
    pub profile: String,
    pub state: ConnectionState,
    /// Upstream model IDs of its last successful discovery; `None` before the first, when an
    /// explicitly configured model may be used with its declared capabilities.
    pub discovered: Option<BTreeSet<String>>,
}

/// The connections the router may route through.
pub trait ConnectionDirectory: Send + Sync {
    /// Every configured connection, in a stable order, as of `read` (the routing decision's own
    /// snapshot or unit of work).
    fn connections(&self, read: &dyn RouterRead) -> Result<Vec<ConnectionStatus>, StorageError>;
}

/// No provider connection is configured (compositions without the connection service).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoConnections;
impl ConnectionDirectory for NoConnections {
    fn connections(&self, _: &dyn RouterRead) -> Result<Vec<ConnectionStatus>, StorageError> {
        Ok(Vec::new())
    }
}
